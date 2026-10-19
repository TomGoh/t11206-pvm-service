#!/bin/sh
# Installs the x-kernel VM control service on a userdebug T11206 tablet with an unlocked
# bootloader the way a product image carries it, through the remount overlay of /system_ext:
# init starts the service at boot, the driver loads on the first start, SELinux policy included.
# Nothing is built or flashed; the original partition stays below the overlay.
#
#   install.sh verity              disable verity (enables the overlay); then reboot once
#   install.sh files BUILD_DIR     install (below); then reboot: init compiles the policy and
#                                  starts the service
#   install.sh status              the service, the driver and the VM
#   install.sh remove              take our files and SELinux blocks out again; then reboot
#   install.sh undo                enable verity again, which drops the whole overlay; reboot
#
# BUILD_DIR holds xkernel_vm_service, xkernel_vmctl, xkernel_crosvm_launcher and xkernel_vm_loader
# (the Android build of this repository). From the device (environment overrides): CROSVM, the
# crosvm build with the hypervisor's backend; MODULES_DIR, the driver's .ko files; KERNEL and
# DUMMY, the guest kernel and its read-only disk; ROOTFS (optional), the template of the guest's
# root file system, which the service copies to /data/misc/xkernel_vm on a first start. The
# writable disk and state stay in /data/misc/xkernel_vm, as etc/xkernel_vm.json says.
#
# MONITOR_APK (optional, a signed APK on this host): the monitor app (OS之OS监控软件,
# com.kylinos.monitor), installed as the privileged system app /system_ext/priv-app/KylinMonitor
# the way the product image carries it. A privileged app holds READ_VM_STATS from boot (the
# privapp allowlist covers only platform permissions), so the monitor works without pm grant.
# Uninstall a copy installed with adb first, or it stays as an update of the system app.
#
# `files` copies the programs, the init script, the instance config, the guest images and the
# modules to their product locations and merges this repository's SELinux policy into
# /system_ext/etc/selinux: system_ext_additions.cil is appended to system_ext_sepolicy.cil, the
# three contexts files get ../../sepolicy/private's entries, and the sha256 file changes so init
# compiles the policy at boot instead of using the precompiled one. It then compiles the result
# with the device's secilc exactly as init will (on-device.sh compile_as_init) and restores the
# original SELinux files if that fails, so a reboot never meets a policy that does not compile.
# Run as often as needed: our earlier block is removed before the new one goes in.
set -eu
SERIAL=${SERIAL:-T11206HXH6600007}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
ST=/data/local/tmp/xkvm-install

device() {
    adb -s "$SERIAL" shell "su 0 env CROSVM=${CROSVM:-} MODULES_DIR=${MODULES_DIR:-} KERNEL=${KERNEL:-} DUMMY=${DUMMY:-} ROOTFS=${ROOTFS:-} sh $ST/on-device.sh $1"
}

stage() {
    adb -s "$SERIAL" shell "su 0 sh -c 'rm -rf $ST; mkdir -p $ST; chmod 0777 $ST'"
    for f in "$HERE/on-device.sh" "$HERE/system_ext_additions.cil" \
        "$REPO/etc/init/xkernel_vm_service.rc" "$REPO/etc/xkernel_vm.json" "$REPO/etc/modules.load" "$@"; do
        adb -s "$SERIAL" push "$f" $ST/ > /dev/null
    done
    for c in file property service; do
        adb -s "$SERIAL" push "$REPO/sepolicy/private/${c}_contexts" $ST/${c}_contexts > /dev/null
    done
}

case "${1:-}" in
verity)
    adb -s "$SERIAL" shell "su 0 disable-verity"
    echo "reboot the tablet, then: install.sh files BUILD_DIR"
    ;;
files)
    build=${2:?BUILD_DIR}
    stage "$build/xkernel_vm_service" "$build/xkernel_vmctl" "$build/xkernel_crosvm_launcher" \
        "$build/xkernel_vm_loader"
    if [ -n "${MONITOR_APK:-}" ]; then
        adb -s "$SERIAL" push "$MONITOR_APK" $ST/monitor.apk > /dev/null
    fi
    device files
    ;;
remove)
    stage
    device remove
    ;;
status)
    stage
    device status
    ;;
undo)
    adb -s "$SERIAL" shell "su 0 enable-verity"
    echo "reboot the tablet: the overlay with all our files is gone after it"
    ;;
*)
    sed -n '2,30p' "$0"
    exit 2
    ;;
esac
