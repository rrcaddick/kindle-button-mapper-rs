use log::{info, warn};
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use nix::{ioctl_none, ioctl_read_buf, ioctl_write_int};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{self, Command};
use std::sync::{Mutex, OnceLock};
use std::thread;

const UINPUT_DEV: &str = "/dev/uinput";
const TARGET_FILE: &str = "/var/run/kindle-button-mapper-key-target";
const FIFO_PATH: &str = "/var/run/kindle-button-mapper-key.fifo";
const FIFO_OWNER: &str = "/var/run/kindle-button-mapper-key-owner";
const SYSFS_INPUT: &str = "/sys/devices/virtual/input";
const DEV_INPUT: &str = "/dev/input";
const DEV_NAME: &[u8] = b"kindle-button-mapper";
const DEV_NAME_STR: &str = "kindle-button-mapper";

const UINPUT_MAX_NAME_SIZE: usize = 80;
const ABS_CNT: usize = 64;
const EV_KEY: u16 = 0x01;
const KEY_UP: u16 = 103;
const KEY_PAGEUP: u16 = 104;
const KEY_DOWN: u16 = 108;
const KEY_PAGEDOWN: u16 = 109;
const EV_SYN: u16 = 0x00;
const SYN_REPORT: u16 = 0x00;

// Multitouch axes, for the swipe page turn. No ABS_MT_SLOT and no BTN_TOUCH:
// the panel reports neither, so sending them only tells the X driver a story
// its own hardware never would.
const EV_ABS: u16 = 0x03;
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_WIDTH_MAJOR: u16 = 0x32;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;

/// A page turn's shape, taken from a recording of a real finger on the panel
/// rather than from the spec.
///
/// The sample spacing is the number that matters: the panel reports about
/// every 15ms, and the X multitouch driver is happiest fed at the rate it
/// expects. Fourteen samples at that spacing is a ~220ms swipe, inside the
/// 300-600ms a human takes but still quick enough not to feel laggy.
const SWIPE_STEPS: i32 = 14;
const SWIPE_STEP_MS: u64 = 16;
const SWIPE_FROM_PCT: i32 = 85;
const SWIPE_TO_PCT: i32 = 15;
const SWIPE_Y_PCT: i32 = 50;

/// The contact id every real touch on this panel carries.
///
/// Not a counter. The hardware reports 0 for the contact and -1 to release it,
/// and never anything else — a recording of seven finger gestures used exactly
/// {0, -1}. Handing the X multitouch driver a fresh id per swipe instead makes
/// it believe an unbounded number of distinct fingers have touched the screen,
/// and after a handful it segfaults and takes the whole framework down with
/// it. Emitting ABS_MT_SLOT has the same flavour of problem: the panel never
/// sends it, so neither do we.
const TOUCH_ID: i32 = 0;
const TOUCH_RELEASE: i32 = -1;

ioctl_none!(ui_dev_create, b'U', 1);
ioctl_write_int!(ui_set_evbit, b'U', 100);
ioctl_write_int!(ui_set_keybit, b'U', 101);
ioctl_read_buf!(ui_get_sysname, b'U', 44, u8);

// Kernel layout: the timestamp is a pair of kernel longs, not libc time_t —
// musl 1.2 widened time_t to 64 bits on 32-bit ARM, which would make this
// struct 8 bytes too long and every write fail with EINVAL.
#[repr(C)]
struct InputEvent {
    tv_sec: isize,
    tv_usec: isize,
    kind: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
struct InputId {
    bustype: u16,
    vendor: u16,
    product: u16,
    version: u16,
}

#[repr(C)]
struct UinputUserDev {
    name: [u8; UINPUT_MAX_NAME_SIZE],
    id: InputId,
    ff_effects_max: u32,
    absmax: [i32; ABS_CNT],
    absmin: [i32; ABS_CNT],
    absfuzz: [i32; ABS_CNT],
    absflat: [i32; ABS_CNT],
}

pub fn try_init() -> Option<File> {
    if let Err(e) = ensure_uinput_node() {
        warn!("{} unavailable: {} — keyboard mappings will not inject events", UINPUT_DEV, e);
        return None;
    }

    let file = match create_device() {
        Ok(f) => f,
        Err(e) => {
            warn!("uinput device create failed: {} — keyboard mappings will not inject events", e);
            return None;
        }
    };

    match dev_node(&file) {
        Ok(path) => {
            let s = path.display().to_string();
            if let Err(e) = fs::write(TARGET_FILE, &s) {
                warn!("Cannot write {}: {}", TARGET_FILE, e);
            } else {
                info!("Virtual keyboard at {} (target written to {})", s, TARGET_FILE);
            }
        }
        Err(e) => warn!("Cannot resolve virtual keyboard node: {}", e),
    }

    Some(file)
}

/// Shared by the FIFO thread and in-process callers.
static INJECTOR: OnceLock<Mutex<Injector>> = OnceLock::new();

struct Injector {
    dev: Option<File>,
    pager: Pager,
}

/// Inject a key name, a key code, `page_next` or `page_prev`. False means there
/// is nothing to inject into, so the caller can fall back to a tap.
pub fn inject(request: &str) -> bool {
    let Some(injector) = INJECTOR.get() else {
        return false;
    };
    let mut injector = injector.lock().unwrap_or_else(|p| p.into_inner());
    injector.handle(request.trim());
    true
}

/// Whether there is a uinput keyboard to emit into at all. Passthrough is a
/// promise the daemon cannot keep without one.
pub fn available() -> bool {
    INJECTOR
        .get()
        .map(|i| i.lock().unwrap_or_else(|p| p.into_inner()).dev.is_some())
        .unwrap_or(false)
}

/// Re-emit a key the mapper grabbed but has no mapping for, so a device the
/// mapper owns exclusively still types. `value` is evdev's own: 1 press,
/// 0 release, 2 repeat. False means there is no uinput keyboard to emit into,
/// in which case the key is simply lost and the caller should say so.
pub fn forward(code: u16, value: i32) -> bool {
    let Some(injector) = INJECTOR.get() else {
        return false;
    };
    let mut injector = injector.lock().unwrap_or_else(|p| p.into_inner());
    let Some(ref mut dev) = injector.dev else {
        return false;
    };
    if let Err(e) = write_event(dev, EV_KEY, code, value)
        .and_then(|()| write_event(dev, EV_SYN, SYN_REPORT, 0))
    {
        warn!("Forwarding key {} failed: {}", code, e);
        return false;
    }
    true
}

/// Serve key injection requests on a FIFO, one key name (or code) per line.
///
/// scripts/key.sh writes to it, so nothing on the device needs an external
/// injector binary — evemu-event is not shipped on stock firmware.
///
/// `dev` is the uinput keyboard, which is absent on firmware without the
/// driver. Page turns still go in through the page buttons there, so the FIFO
/// is only pointless when neither exists, and it stays unopened in that case so
/// key.sh fails and scripts/kindle.sh can fall back to a tap.
pub fn serve(dev: Option<File>) {
    let pager = Pager::find();
    if dev.is_none() && matches!(pager, Pager::VirtualKeyboard) {
        warn!("No page buttons and no uinput keyboard — nothing to inject into, use the tap page turn actions");
        return;
    }
    if INJECTOR.set(Mutex::new(Injector { dev, pager })).is_err() {
        warn!("Virtual keyboard already serving");
        return;
    }

    // A FIFO left behind by a crashed daemon would block writers forever.
    let _ = fs::remove_file(FIFO_PATH);
    if let Err(e) = mkfifo(FIFO_PATH, Mode::from_bits_truncate(0o600)) {
        warn!("Cannot create {}: {} — key injection unavailable", FIFO_PATH, e);
        return;
    }
    // key.sh checks this pid before writing: opening a FIFO nobody reads blocks.
    if let Err(e) = fs::write(FIFO_OWNER, process::id().to_string()) {
        warn!("Cannot write {}: {}", FIFO_OWNER, e);
    }
    info!("Key injection FIFO at {}", FIFO_PATH);
    thread::Builder::new()
        .name("keyfifo".into())
        .spawn(fifo_loop)
        .map(|_| ())
        .unwrap_or_else(|e| warn!("Cannot spawn key FIFO thread: {}", e));
}

fn fifo_loop() {
    loop {
        // Read/write so a writer closing does not end the loop, and so key.sh
        // never blocks on open while the daemon is alive.
        let fifo = match OpenOptions::new().read(true).write(true).open(FIFO_PATH) {
            Ok(f) => f,
            Err(e) => {
                warn!("Cannot open {}: {} — key injection stopped", FIFO_PATH, e);
                return;
            }
        };
        for line in BufReader::new(fifo).lines() {
            match line {
                Ok(l) => {
                    inject(&l);
                }
                Err(e) => {
                    warn!("{} read failed: {}", FIFO_PATH, e);
                    break;
                }
            }
        }
    }
}

impl Injector {
    fn handle(&mut self, line: &str) {
        let Injector { dev, pager } = self;
        match line {
            "" => (),
            "page_next" => pager.turn(dev.as_mut(), KEY_PAGEDOWN, KEY_DOWN, true),
            "page_prev" => pager.turn(dev.as_mut(), KEY_PAGEUP, KEY_UP, false),
            _ => match crate::config::parse_key(line) {
                Some(key) => match dev {
                    Some(vkbd) => {
                        if let Err(e) = tap(vkbd, key.code()) {
                            warn!("Injecting {} failed: {}", line, e);
                        }
                    }
                    None => warn!("Cannot inject {}, no uinput keyboard", line),
                },
                None => warn!("Key injection: unknown key {:?}", line),
            },
        }
    }
}

/// The device the reader takes page turns from.
///
/// Kindles with physical page buttons carry them on their own node, and the
/// framework only turns pages for that node, ignoring page keys from any
/// keyboard. Writing to an evdev node injects into the device itself, so a
/// page turn goes in as if the button had been pressed.
///
/// Models without the buttons get a swipe across the touchscreen. They used
/// to get KEY_DOWN/KEY_UP on the virtual keyboard, but the native reader
/// ignores keys entirely — measured on a Paperwhite 4 running 5.18, where
/// fourteen different keycodes were injected, verified to reach a properly
/// tagged keyboard node, and not one moved the page. Nothing upstream could
/// tell, because injecting *succeeds*: the key is delivered, it just does
/// nothing, so the page turn silently did nothing too.
///
/// A swipe rather than a tap because a tap is a stationary contact: two in
/// quick succession read as a double tap and a slow one as a long press,
/// either of which selects a word and opens the dictionary instead of turning
/// the page. A moving contact can be neither.
enum Pager {
    Buttons { path: PathBuf, node: Option<File> },
    Touchscreen(Touchscreen),
    VirtualKeyboard,
}

impl Pager {
    fn find() -> Self {
        if let Some(path) = page_button_node() {
            info!("Page turns go to the page buttons at {}", path.display());
            return Pager::Buttons { path, node: None };
        }
        match Touchscreen::find() {
            Some(touch) => {
                info!(
                    "No page buttons found, page turns swipe the touchscreen at {} ({}x{})",
                    touch.path.display(),
                    touch.max_x,
                    touch.max_y
                );
                Pager::Touchscreen(touch)
            }
            None => {
                info!("No page buttons or touchscreen found, page turns go to the virtual keyboard");
                Pager::VirtualKeyboard
            }
        }
    }

    fn turn(&mut self, vkbd: Option<&mut File>, button_code: u16, kbd_code: u16, forward: bool) {
        match self {
            Pager::Buttons { path, node } => {
                if node.is_none() {
                    match OpenOptions::new().write(true).open(&path) {
                        Ok(f) => *node = Some(f),
                        Err(e) => {
                            warn!("Cannot open {}: {}", path.display(), e);
                            return;
                        }
                    }
                }
                let f = node.as_mut().expect("opened above");
                if let Err(e) = tap(f, button_code) {
                    warn!("Page turn on {} failed: {}", path.display(), e);
                    // Reopen next time, the node may have gone away.
                    *node = None;
                }
            }
            Pager::Touchscreen(touch) => {
                if let Err(e) = touch.swipe(forward) {
                    warn!("Page turn swipe on {} failed: {}", touch.path.display(), e);
                    // Reopen next time, the node may have gone away.
                    touch.node = None;
                }
            }
            Pager::VirtualKeyboard => match vkbd {
                Some(vkbd) => {
                    if let Err(e) = tap(vkbd, kbd_code) {
                        warn!("Page turn failed: {}", e);
                    }
                }
                // serve() does not open the FIFO in this case.
                None => warn!("Page turn failed, no uinput keyboard"),
            },
        }
    }
}

/// The panel, and the geometry a page-turn swipe needs.
struct Touchscreen {
    path: PathBuf,
    node: Option<File>,
    max_x: i32,
    max_y: i32,
    /// What a fingertip reports for pressure and contact size. Real touches on
    /// the Paperwhite 4 land between 16 and 30, so a middling value passes for
    /// one; panels with a smaller range get something inside theirs.
    touch_size: i32,
}

impl Touchscreen {
    fn find() -> Option<Self> {
        let mut found: Vec<PathBuf> = fs::read_dir(DEV_INPUT)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("event"))
            })
            .filter(|p| evdev::Device::open(p).is_ok_and(is_touchscreen))
            .collect();
        found.sort();

        for path in found {
            let Ok(dev) = evdev::Device::open(&path) else {
                continue;
            };
            let Ok(abs) = dev.get_abs_state() else {
                continue;
            };
            let max_x = abs[ABS_MT_POSITION_X as usize].maximum;
            let max_y = abs[ABS_MT_POSITION_Y as usize].maximum;
            let max_touch = abs[ABS_MT_TOUCH_MAJOR as usize].maximum;
            if max_x > 0 && max_y > 0 {
                return Some(Touchscreen {
                    path,
                    node: None,
                    max_x,
                    max_y,
                    touch_size: if max_touch > 0 { max_touch.min(24) } else { 24 },
                });
            }
        }
        None
    }

    /// Drag one contact across the screen: right-to-left turns forward, the
    /// same way a finger does.
    ///
    /// Shaped after a recording of a real finger: the opening report carries
    /// the contact and its size, each later report carries only the
    /// coordinate that moved, and the last one just releases. Sending more
    /// than the panel itself would is what upsets the X driver.
    fn swipe(&mut self, forward: bool) -> io::Result<()> {
        if self.node.is_none() {
            self.node = Some(OpenOptions::new().write(true).open(&self.path)?);
        }
        let (from_pct, to_pct) = if forward {
            (SWIPE_FROM_PCT, SWIPE_TO_PCT)
        } else {
            (SWIPE_TO_PCT, SWIPE_FROM_PCT)
        };
        let x0 = self.max_x * from_pct / 100;
        let x1 = self.max_x * to_pct / 100;
        let y = self.max_y * SWIPE_Y_PCT / 100;
        let touch = self.touch_size;

        let dev = self.node.as_mut().expect("opened above");
        write_event(dev, EV_ABS, ABS_MT_TRACKING_ID, TOUCH_ID)?;
        write_event(dev, EV_ABS, ABS_MT_PRESSURE, touch)?;
        write_event(dev, EV_ABS, ABS_MT_POSITION_X, x0)?;
        write_event(dev, EV_ABS, ABS_MT_POSITION_Y, y)?;
        write_event(dev, EV_ABS, ABS_MT_TOUCH_MAJOR, touch)?;
        write_event(dev, EV_ABS, ABS_MT_WIDTH_MAJOR, touch)?;
        write_event(dev, EV_SYN, SYN_REPORT, 0)?;

        // One sample per report, spaced at the panel's own report rate.
        // Batching them into a single write would give every position the
        // same kernel timestamp, and a contact that crosses the screen
        // instantly is not a swipe.
        let step = std::time::Duration::from_millis(SWIPE_STEP_MS);
        for i in 1..=SWIPE_STEPS {
            thread::sleep(step);
            write_event(dev, EV_ABS, ABS_MT_POSITION_X, x0 + (x1 - x0) * i / SWIPE_STEPS)?;
            write_event(dev, EV_SYN, SYN_REPORT, 0)?;
        }

        thread::sleep(step);
        write_event(dev, EV_ABS, ABS_MT_TRACKING_ID, TOUCH_RELEASE)?;
        write_event(dev, EV_SYN, SYN_REPORT, 0)
    }
}

/// A real panel: absolute multitouch coordinates on a device you touch
/// directly, and not something we made ourselves.
///
/// INPUT_PROP_DIRECT is the check that matters, not the bus. Panels do not sit
/// on BUS_HOST the way the page buttons do — the Paperwhite 4's goodix-ts is
/// BUS_I2C — while a trackpad or a gamepad carries absolute axes without ever
/// claiming to be the surface under the image.
fn is_touchscreen(dev: evdev::Device) -> bool {
    if dev.name() == Some(DEV_NAME_STR) {
        return false;
    }
    if !dev.properties().contains(evdev::PropType::DIRECT) {
        return false;
    }
    dev.supported_absolute_axes().is_some_and(|a| {
        a.contains(evdev::AbsoluteAxisType::ABS_MT_POSITION_X)
            && a.contains(evdev::AbsoluteAxisType::ABS_MT_POSITION_Y)
    })
}

fn page_button_node() -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(DEV_INPUT)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("event"))
        })
        .filter(|p| evdev::Device::open(p).is_ok_and(is_page_buttons))
        .collect();
    found.sort();
    found.into_iter().next()
}

fn is_page_buttons(dev: evdev::Device) -> bool {
    if dev.name() == Some(DEV_NAME_STR) {
        return false;
    }
    // Built into the device, so a paired keyboard or gamepad that happens to
    // carry page keys is never mistaken for the reader's own buttons.
    if dev.input_id().bus_type() != evdev::BusType::BUS_HOST {
        return false;
    }
    dev.supported_keys().is_some_and(|k| {
        k.contains(evdev::Key::KEY_PAGEUP)
            && k.contains(evdev::Key::KEY_PAGEDOWN)
            && !k.contains(evdev::Key::KEY_A)
    })
}

fn tap(dev: &mut File, code: u16) -> io::Result<()> {
    write_event(dev, EV_KEY, code, 1)?;
    write_event(dev, EV_SYN, SYN_REPORT, 0)?;
    write_event(dev, EV_KEY, code, 0)?;
    write_event(dev, EV_SYN, SYN_REPORT, 0)
}

fn write_event(dev: &mut File, kind: u16, code: u16, value: i32) -> io::Result<()> {
    // The kernel stamps the time itself, so leaving it zero is fine.
    let ev = InputEvent {
        tv_sec: 0,
        tv_usec: 0,
        kind,
        code,
        value,
    };
    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&ev as *const InputEvent).cast::<u8>(),
            mem::size_of::<InputEvent>(),
        )
    };
    dev.write_all(bytes)
}

fn create_device() -> io::Result<File> {
    let file = OpenOptions::new().read(true).write(true).open(UINPUT_DEV)?;
    let fd = file.as_raw_fd();

    unsafe { ui_set_evbit(fd, EV_KEY as u32 as _) }?;
    for code in supported_keys() {
        unsafe { ui_set_keybit(fd, code as _) }?;
    }

    // Setup via a uinput_user_dev write rather than UI_DEV_SETUP: that ioctl
    // only exists on Linux 4.5+, and Kindles through 10th gen run 3.0/4.1
    // kernels that answer it with EINVAL. The write path works on both.
    let mut setup = UinputUserDev {
        name: [0; UINPUT_MAX_NAME_SIZE],
        id: InputId {
            bustype: 0x03, // BUS_USB
            vendor: 0x1234,
            product: 0x5678,
            version: 0x111,
        },
        ff_effects_max: 0,
        absmax: [0; ABS_CNT],
        absmin: [0; ABS_CNT],
        absfuzz: [0; ABS_CNT],
        absflat: [0; ABS_CNT],
    };
    setup.name[..DEV_NAME.len()].copy_from_slice(DEV_NAME);

    let bytes = unsafe {
        std::slice::from_raw_parts(
            (&setup as *const UinputUserDev).cast::<u8>(),
            mem::size_of::<UinputUserDev>(),
        )
    };
    (&file).write_all(bytes)?;

    unsafe { ui_dev_create(fd) }?;
    Ok(file)
}

fn dev_node(file: &File) -> io::Result<PathBuf> {
    let mut buf = [0u8; 64];
    let len = unsafe { ui_get_sysname(file.as_raw_fd(), &mut buf) }? as usize;
    let bytes = &buf[..len.min(buf.len())];
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    let sysname = std::str::from_utf8(&bytes[..end])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let sysdir = Path::new(SYSFS_INPUT).join(sysname);
    for entry in fs::read_dir(&sysdir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy().into_owned();
        if !name.starts_with("event") {
            continue;
        }
        let path = Path::new(DEV_INPUT).join(&name);
        ensure_event_node(&path, &sysdir.join(&name))?;
        return Ok(path);
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("no event node under {}", sysdir.display()),
    ))
}

fn ensure_uinput_node() -> Result<(), String> {
    if Path::new(UINPUT_DEV).exists() {
        return Ok(());
    }
    // Kernel built with CONFIG_INPUT_UINPUT=y but no devtmpfs node — create it.
    let status = Command::new("mknod")
        .args([UINPUT_DEV, "c", "10", "223"])
        .status()
        .map_err(|e| format!("mknod missing: {}", e))?;
    if !status.success() {
        return Err(format!("mknod exit {}", status.code().unwrap_or(-1)));
    }
    let _ = Command::new("chmod").args(["600", UINPUT_DEV]).status();
    Ok(())
}

fn ensure_event_node(path: &Path, sysdir: &Path) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let dev = fs::read_to_string(sysdir.join("dev"))?;
    let (major, minor) = dev
        .trim()
        .split_once(':')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("bad dev {:?}", dev)))?;
    let status = Command::new("mknod")
        .args([&path.display().to_string(), "c", major, minor])
        .status()?;
    // devtmpfs can beat us to the node between the check above and here, and
    // that mknod failure is not one worth losing the keyboard over.
    if !status.success() && !path.exists() {
        return Err(io::Error::other(format!(
            "mknod {} exit {}",
            path.display(),
            status.code().unwrap_or(-1)
        )));
    }
    let _ = Command::new("chmod").args(["600", &path.display().to_string()]).status();
    Ok(())
}

fn supported_keys() -> impl Iterator<Item = u32> {
    // All KEY_* codes. The BTN_* ranges (0x100-0x15f mouse/gamepad,
    // 0x2c0+ trigger-happy) are skipped so the device enumerates as a
    // plain keyboard.
    (1..0x100).chain(0x160..0x2c0)
}
