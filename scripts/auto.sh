#!/bin/sh
# Reader control that follows whatever is on screen.
# Usage: auto.sh <command> [args...]
#
# Commands:
#   next_page         - Turn to next page
#   prev_page         - Turn to previous page
#   brightness <n>    - Adjust frontlight (positive=up, negative=down)
#   brightness_toggle - Toggle frontlight off/on
#   menu              - KOReader menu, native toolbar
#
# KOReader answers on its HTTP Inspector port when it is up, so a reachable
# port means it is the reader in front of you. Everything else, including the
# native-only commands (home, back, toolbar), goes to kindle.sh.

DIR=$(dirname "$0")
KOREADER_PROBE="http://localhost:8080/"

koreader_up() {
    curl -s --connect-timeout 1 -o /dev/null "$KOREADER_PROBE" 2>/dev/null
}

CMD="$1"
case "$CMD" in
    next_page|prev_page|brightness|brightness_toggle|night_mode|font_up|font_down|toggle_status_bar|rotate)
        koreader_up && exec "$DIR/koreader.sh" "$@"
        ;;
    menu)
        if koreader_up; then
            exec "$DIR/koreader.sh" menu
        fi
        exec "$DIR/kindle.sh" toolbar
        ;;
    "")
        echo "Usage: $0 <command> [args...]"
        echo "Commands: next_page, prev_page, brightness <n>, brightness_toggle, menu"
        exit 1
        ;;
esac

exec "$DIR/kindle.sh" "$@"
