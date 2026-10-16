#!/usr/bin/env bash
# Host side of the 2026-10-16 pvmfw trial on T11206 (see on-device-pvmfw.sh).
#
#   pvmfw-trial.sh stage              push the firmware, kernels, instance disk and device script;
#                                     stop the control service's VM; copy its disks for the trial
#   pvmfw-trial.sh start TAG [KERNEL] start the trial VM (KERNEL: a path on the tablet)
#   pvmfw-trial.sh log TAG            print the run's log
#   pvmfw-trial.sh stop | status
#
# Inputs in INPUTS (default: the record's inputs/, docs/artifacts/t11206-pvmfw-trial-20261016):
# kernel-d36aa46b-sm2signed.bin (sign-xkernel.sh), kernel-tablet-d36aa46b.bin (the unsigned kernel
# the service runs, the negative control), instance-empty.img (make-instance-img.py); and FW, the
# vendor pvmfw kylin100-xkernel-boots/prebuilt/custom_pvmfw (pvmfw.bin + a test BCC, 2e0454e).
set -euo pipefail
SERIAL=${SERIAL:-T11206HXH6600007}
R=$(cd "$(dirname "$0")/.." && pwd)
IN=${INPUTS:-$R/inputs}
HERE=$(cd "$(dirname "$0")" && pwd)
D=/data/local/tmp/xkpvm-fw
FW=${FW:-/Volumes/Dev/Code/kylin100-xkernel-boots/prebuilt/custom_pvmfw}

adb_() { timeout 120 adb -s "$SERIAL" "$@"; }
dev() { adb_ shell "su 0 env ${ENVS:-} sh $D/on-device-pvmfw.sh $*"; }

case "${1:-}" in
stage)
    adb_ shell "su 0 sh -c 'mkdir -p $D && chmod 0777 $D'"
    adb_ push "$FW" $D/custom_pvmfw
    adb_ push "$IN/kernel-d36aa46b-sm2signed.bin" $D/kernel-signed.bin
    adb_ push "$IN/kernel-tablet-d36aa46b.bin" $D/kernel-unsigned.bin
    adb_ push "$IN/instance-empty.img" $D/instance-empty.img
    adb_ push "$HERE/on-device-pvmfw.sh" $D/on-device-pvmfw.sh
    echo "== stop the control service's VM"
    adb_ shell "su 0 /system_ext/bin/xkernel_vmctl stop" || true
    sleep 3
    adb_ shell "su 0 /system_ext/bin/xkernel_vmctl status"
    adb_ shell "su 0 sh -c 'pgrep -fl crosvm || echo no crosvm running'"
    adb_ shell "su 0 sh -c 'cp /system_ext/etc/xkernel_vm/dummy.img $D/dummy.img && \
        cp /data/misc/xkernel_vm/rootfs.img $D/rootfs.img && \
        sha256sum $D/custom_pvmfw $D/kernel-signed.bin $D/kernel-unsigned.bin $D/instance-empty.img \
        $D/dummy.img /data/local/tmp/xhyper-new/crosvm-xhyper; lsmod | grep -c xhyper; ls -lZ /dev/xhyper'"
    ;;
start) dev start "${2:?TAG}" ${3:-} ;;
log) adb_ shell "su 0 cat $D/run-${2:?TAG}.log" ;;
stop) dev stop ;;
status) dev status ;;
*) sed -n '2,13p' "$0"; exit 2 ;;
esac
