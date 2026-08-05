//! In-process config reload.
//!
//! Restarting the daemon to pick up a config change destroys the uinput
//! keyboard, and anything holding that node open sees the device disappear.
//! KOReader reacts to that by rebuilding its UI, which throws the user out of
//! whatever menu they were editing the mapping from. So SIGHUP re-reads the
//! config in place instead, and the uinput node outlives it.

use crate::config::{Config, DeviceConfig};
use log::{info, warn};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

static REQUESTED: AtomicBool = AtomicBool::new(false);
/// Bumped once per applied reload. Workers compare against their own copy.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static DEVICES: OnceLock<Mutex<Vec<DeviceConfig>>> = OnceLock::new();

fn devices() -> &'static Mutex<Vec<DeviceConfig>> {
    DEVICES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Signal handler. Only an atomic store, which is async-signal-safe.
pub extern "C" fn handle_sighup(_: i32) {
    REQUESTED.store(true, Ordering::SeqCst);
}

pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

pub fn publish(config: &Config) {
    *devices().lock().unwrap_or_else(|p| p.into_inner()) = config.devices.clone();
}

/// The device's current config, or None if it was removed from the file.
pub fn device_config(id: &str) -> Option<DeviceConfig> {
    devices()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .find(|d| d.id == id)
        .cloned()
}

/// Re-read the config if SIGHUP asked for it. True when a reload was applied.
///
/// Devices added to the file since startup have no worker and are not picked
/// up here, that still needs a restart, but a mapping change on a device that
/// already has one applies immediately.
pub fn poll(config_path: &str) -> bool {
    if !REQUESTED.swap(false, Ordering::SeqCst) {
        return false;
    }
    match Config::load(config_path) {
        Ok(new) => {
            let known: Vec<String> = devices()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .iter()
                .map(|d| d.id.clone())
                .collect();
            for d in &new.devices {
                if !known.contains(&d.id) {
                    warn!("[{}] new device in config, needs a restart to be watched", d.id);
                }
            }
            publish(&new);
            let gen = GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
            info!("Config reloaded in place (generation {})", gen);
            true
        }
        Err(e) => {
            warn!("Reload failed, keeping the running config: {}", e);
            false
        }
    }
}
