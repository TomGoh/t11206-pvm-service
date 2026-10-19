#!/bin/bash
# Put xkload into an x-kernel root fs image (ext4), offline and without root: e2fsprogs' debugfs edits the image file.
#   /root/xkload.sh          root:root 0755
#   /usr/local/bin/xkload -> /root/xkload.sh
# Use it on the template the image ships (/system_ext/etc/xkernel_vm/rootfs.img, see product/README.md) before it goes
# into the prebuilts directory: a VM whose writable disk is created from the template then has `xkload` from the start.
# Running it twice is safe (the two files are replaced; only timestamps change). e2fsck -fn must then report no problems.
#   guest/add-to-rootfs.sh ROOTFS_IMG
set -Eeo pipefail
IMG=$1
HERE=$(cd "$(dirname "$0")" && pwd)
SRC=$HERE/xkload.sh
[ -f "$IMG" ] || { echo "usage: $0 ROOTFS_IMG" >&2; exit 2; }
command -v debugfs >/dev/null || { echo "debugfs (e2fsprogs) is required" >&2; exit 1; }
d() { debugfs -R "$1" "$IMG" 2>/dev/null; }
[ -n "$(d 'stat /root' | grep 'Type: directory')" ] || { echo "$IMG has no /root directory" >&2; exit 1; }

cmds=$(mktemp)
trap 'rm -f "$cmds"' EXIT
{
  d 'stat /usr/local' | grep -q 'Type: directory' || echo "mkdir /usr/local"
  d 'stat /usr/local/bin' | grep -q 'Type: directory' || echo "mkdir /usr/local/bin"
  d 'stat /root/xkload.sh' | grep -q 'Inode:' && echo "rm /root/xkload.sh"
  d 'stat /usr/local/bin/xkload' | grep -q 'Inode:' && echo "rm /usr/local/bin/xkload"
  echo "write $SRC /root/xkload.sh"
  echo "sif /root/xkload.sh mode 0100755"
  echo "sif /root/xkload.sh uid 0"
  echo "sif /root/xkload.sh gid 0"
  echo "symlink /usr/local/bin/xkload /root/xkload.sh"
} > "$cmds"
debugfs -w -f "$cmds" "$IMG" > /dev/null 2>&1

# Check what landed, not what was asked for.
st=$(d 'stat /root/xkload.sh')
echo "$st" | grep -q 'Mode:  0755' && echo "$st" | grep -qE 'User: +0 +Group: +0' || { echo "wrong mode or owner:"; echo "$st" | head -3; exit 1; }
d 'stat /usr/local/bin/xkload' | grep -q 'Fast link dest: "/root/xkload.sh"' || { echo "link /usr/local/bin/xkload missing"; exit 1; }
t=$(mktemp); d "dump /root/xkload.sh $t" > /dev/null
[ "$(sha256sum "$t" | cut -d' ' -f1)" = "$(sha256sum "$SRC" | cut -d' ' -f1)" ] || { rm -f "$t"; echo "content differs"; exit 1; }
rm -f "$t"
e2fsck -fn "$IMG" > /dev/null 2>&1 || { echo "e2fsck reports problems in $IMG"; exit 1; }
echo "xkload added to $IMG ($(sha256sum "$IMG" | cut -d' ' -f1))"
