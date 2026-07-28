#!/bin/sh
# Inject a keyboard event into the daemon's virtual uinput keyboard.
# Usage: key.sh KEY_NAME

KEY="$1"
[ -z "$KEY" ] && { echo "Usage: $0 KEY_NAME" >&2; exit 1; }

FIFO="/var/run/kindle-button-mapper-key.fifo"
FIFO_OWNER="/var/run/kindle-button-mapper-key-owner"

# The daemon owns the uinput fd, so it does the injecting: writing the key name
# to its FIFO works on stock firmware, which has no evemu-event. The pid check
# keeps a FIFO left behind by a crashed daemon from blocking this write.
if [ -p "$FIFO" ] && [ -r "$FIFO_OWNER" ] && [ -d "/proc/$(cat "$FIFO_OWNER")" ]; then
    printf '%s\n' "$KEY" > "$FIFO" && exit 0
fi

# Fallback: an older daemon publishes the event node but no FIFO.
DEV="$KEY_TARGET_DEV"
[ -z "$DEV" ] && [ -r /var/run/kindle-button-mapper-key-target ] && DEV=$(cat /var/run/kindle-button-mapper-key-target)
[ -z "$DEV" ] && [ -r /etc/kindle-button-mapper-key-target ] && DEV=$(cat /etc/kindle-button-mapper-key-target)

if [ -z "$DEV" ] || [ ! -e "$DEV" ]; then
    echo "key.sh: virtual keyboard not running. Restart the kindle-button-mapper daemon." >&2
    exit 1
fi

if ! command -v evemu-event >/dev/null 2>&1; then
    echo "key.sh: daemon is too old for FIFO injection and evemu-event is not installed. Update kindle-button-mapper." >&2
    exit 1
fi

evemu-event "$DEV" --type EV_KEY --code "$KEY" --value 1 --sync
evemu-event "$DEV" --type EV_KEY --code "$KEY" --value 0 --sync
