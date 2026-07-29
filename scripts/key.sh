#!/bin/sh
# Ask the daemon to inject an input event.
# Usage: key.sh KEY_NAME | page_next | page_prev
#
# A key name goes into the daemon's virtual uinput keyboard. page_next and
# page_prev let the daemon pick the device, since Kindles with physical page
# buttons only turn pages for those.

KEY="$1"
[ -z "$KEY" ] && { echo "Usage: $0 KEY_NAME|page_next|page_prev" >&2; exit 1; }

FIFO="/var/run/kindle-button-mapper-key.fifo"
FIFO_OWNER="/var/run/kindle-button-mapper-key-owner"

# The daemon owns the uinput fd, so it does the injecting. Writing the request
# to its FIFO works on stock firmware, which has no evemu-event. The pid check
# keeps a FIFO left behind by a crashed daemon from blocking this write.
if [ -p "$FIFO" ] && [ -r "$FIFO_OWNER" ] && [ -d "/proc/$(cat "$FIFO_OWNER")" ]; then
    printf '%s\n' "$KEY" > "$FIFO" && exit 0
fi

# Fallback: an older daemon publishes the event node but no FIFO. Page turns
# only exist as a daemon request, so map them back to keys here.
case "$KEY" in
    page_next) KEY=KEY_DOWN ;;
    page_prev) KEY=KEY_UP ;;
esac

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
