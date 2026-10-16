#!/bin/sh
# Install (or with --remove, uninstall) the udev rules that let symbioticd open the adapters and
# USBTMC instruments, and apply them to the devices already plugged in. Run with sudo.
#   (none)      the user at the machine's seat gets the devices (70-symbiotic.rules, uaccess)
#   --everyone  every account does too: WSL, a headless machine (71-symbiotic-everyone.rules, 0666)
#   --remove    both rules go
set -eu
dir=/etc/udev/rules.d
here=$(dirname "$0")
if [ "$(id -u)" -ne 0 ]; then
    echo "run it with sudo: sudo $0 $*" >&2
    exit 1
fi
case "${1:-}" in
    --remove) rm -f "$dir/70-symbiotic.rules" "$dir/71-symbiotic-everyone.rules"; echo "removed the rules from $dir" ;;
    "") install -m 0644 "$here/70-symbiotic.rules" "$dir/"; rm -f "$dir/71-symbiotic-everyone.rules"; echo "installed $dir/70-symbiotic.rules (the user at the seat)" ;;
    --everyone)
        install -m 0644 "$here/70-symbiotic.rules" "$here/71-symbiotic-everyone.rules" "$dir/"
        echo "installed $dir/70-symbiotic.rules and 71-symbiotic-everyone.rules (every account)" ;;
    *) echo "usage: $0 [--everyone | --remove]" >&2; exit 2 ;;
esac
udevadm control --reload
# "change", which gives what a rule now sets; a 0666 a rule no longer sets stays on a device plugged
# in now until it is next plugged in (only "add" takes it back, and replaying "add" for every USB
# device would run every other rule's add actions too).
udevadm trigger --subsystem-match=usb --action=change
