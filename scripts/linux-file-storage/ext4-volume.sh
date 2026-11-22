#!/bin/sh
# Create only a bounded image inside the container's private tmpfs.
set -eu
test "${LCXBOX_ISOLATED_EXT4:-}" = 1
truncate -s 64M /disk/image
mkfs.ext4 -q -m 0 -N 1024 /disk/image
loop_device=$(losetup --find)
case "$loop_device" in /dev/loop[0-9]*) ;; *) exit 1 ;; esac
loop_number=${loop_device#/dev/loop}
case "$loop_number" in ''|*[!0-9]*) exit 1 ;; esac
test ! -e "$loop_device"
mknod "$loop_device" b 7 "$loop_number"
# mount owns an autoclear loop association. Namespace teardown also unmounts it.
mount -t ext4 -o "loop=$loop_device,nosuid,nodev,noexec" /disk/image /fixture
trap 'umount /fixture' EXIT
test "$(stat -f -c %T /fixture)" = ext2/ext3
if test "${LCXBOX_EXT4_PREFLIGHT:-}" = 1; then
    echo isolated_ext4_ready
    exit 0
fi
chown "$LCXBOX_TEST_UID:$LCXBOX_TEST_GID" /fixture
setpriv --reuid="$LCXBOX_TEST_UID" --regid="$LCXBOX_TEST_GID" --clear-groups \
    --bounding-set=-all --inh-caps=-all --ambient-caps=-all "$@"
