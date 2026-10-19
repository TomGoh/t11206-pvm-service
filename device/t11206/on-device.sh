#!/system/bin/sh
# The device side of install.sh: runs as root on the tablet, from the staging directory install.sh
# filled. See install.sh for the steps; this file does `files`, `remove` and `status`.
set -eu
ST=$(cd "$(dirname "$0")" && pwd)
E=/system_ext
SE=$E/etc/selinux
MARK="x-kernel VM control service"
CROSVM=${CROSVM:-/data/local/tmp/xhyper-new/crosvm-xhyper}
MODULES_DIR=${MODULES_DIR:-/data/local/tmp/xhyper-new}
KERNEL=${KERNEL:-/data/local/tmp/xkernel-pvm/vm/kernel.bin}
DUMMY=${DUMMY:-/data/local/tmp/xkernel-pvm/vm/dummy.img}
BINS="xkernel_vm_service xkernel_vmctl xkernel_crosvm_launcher xkernel_vm_loader"
# The monitor app (OS之OS监控软件), when install.sh staged one: a privileged system app, as the
# product image carries it, so it holds READ_VM_STATS from boot without pm grant.
MONITOR=$E/priv-app/KylinMonitor
CONTEXTS="file property service"

# The partition's own SELinux file: ours is the block from the marker line to the end.
original() {
    sed "/$MARK/,\$d" "$1"
}

# The precompiled policy is used only while the system_ext hash matches the one it was built
# with, so that recorded hash is also the partition's original one.
original_sha() {
    for p in /odm /vendor; do
        f=$p/etc/selinux/precompiled_sepolicy.system_ext_sepolicy_and_mapping.sha256
        if [ -f $f ]; then cat $f; return; fi
    done
    echo "no precompiled system_ext hash" >&2
    return 1
}

restore_selinux() {
    for c in $CONTEXTS; do
        original $SE/system_ext_${c}_contexts > $ST/ctx.tmp && cp $ST/ctx.tmp $SE/system_ext_${c}_contexts
    done
    original $SE/system_ext_sepolicy.cil > $ST/cil.tmp && cp $ST/cil.tmp $SE/system_ext_sepolicy.cil
    original_sha > $SE/system_ext_sepolicy_and_mapping.sha256
}

# What init runs at boot when the precompiled policy does not match (system/core/init/selinux.cpp):
# the same files in the same order, the kernel's policy version, the genfs file of the vendor's
# genfs labels version if the system has it.
compile_as_init() {
    v=$(cat /vendor/etc/selinux/plat_sepolicy_vers.txt)
    P=/system/etc/selinux
    set -- $P/plat_sepolicy.cil -m -M true -G -N -c "$(cat /sys/fs/selinux/policyvers)" \
        $P/mapping/$v.cil -o "$1" -f /dev/null
    for f in $P/mapping/$v.compat.cil $SE/system_ext_sepolicy.cil $SE/mapping/$v.cil \
        $SE/mapping/$v.compat.cil /product/etc/selinux/product_sepolicy.cil \
        /product/etc/selinux/mapping/$v.cil /vendor/etc/selinux/plat_pub_versioned.cil \
        /vendor/etc/selinux/vendor_sepolicy.cil /odm/etc/selinux/odm_sepolicy.cil; do
        if [ -f $f ]; then set -- "$@" $f; fi
    done
    g=$(cat /vendor/etc/selinux/genfs_labels_version.txt 2>/dev/null || echo 202404)
    if [ -f $P/plat_sepolicy_genfs_$g.cil ]; then set -- "$@" $P/plat_sepolicy_genfs_$g.cil; fi
    /system/bin/secilc "$@"
}

case "${1:-}" in
files)
    remount system_ext
    touch $E/.xkvm-writable && rm $E/.xkvm-writable
    # Copy, then rename: a running service or crosvm keeps its file (no "text file busy").
    for b in $BINS; do cp $ST/$b $E/bin/.$b.new && mv $E/bin/.$b.new $E/bin/$b; done
    cp "$CROSVM" $E/bin/.xkernel_crosvm.new && mv $E/bin/.xkernel_crosvm.new $E/bin/xkernel_crosvm
    for b in $BINS xkernel_crosvm; do chown root:shell $E/bin/$b; chmod 0755 $E/bin/$b; done
    cp $ST/xkernel_vm_service.rc $E/etc/init/xkernel_vm_service.rc
    mkdir -p $E/etc/xkernel_vm/modules
    cp $ST/xkernel_vm.json "$KERNEL" "$DUMMY" $E/etc/xkernel_vm/
    cp $ST/modules.load $E/etc/xkernel_vm/modules/
    for m in $(grep -o '^[A-Za-z0-9_-]*\.ko' $ST/modules.load); do
        cp "$MODULES_DIR/$m" $E/etc/xkernel_vm/modules/
    done
    chown -R root:root $E/etc/init/xkernel_vm_service.rc $E/etc/xkernel_vm
    chmod 0644 $E/etc/init/xkernel_vm_service.rc $E/etc/xkernel_vm/*.* $E/etc/xkernel_vm/modules/*
    chmod 0755 $E/etc/xkernel_vm $E/etc/xkernel_vm/modules
    if [ -f $ST/monitor.apk ]; then
        mkdir -p $MONITOR
        cp $ST/monitor.apk $MONITOR/.KylinMonitor.apk.new && mv $MONITOR/.KylinMonitor.apk.new $MONITOR/KylinMonitor.apk
        chown -R root:root $MONITOR; chmod 0755 $MONITOR; chmod 0644 $MONITOR/KylinMonitor.apk
        echo "monitor app: $MONITOR/KylinMonitor.apk"
    fi

    # SELinux: the partition's files without our earlier block, plus this repository's.
    for c in $CONTEXTS; do
        { original $SE/system_ext_${c}_contexts; echo "# $MARK"; cat $ST/${c}_contexts; } > $ST/ctx.tmp
        cp $ST/ctx.tmp $SE/system_ext_${c}_contexts
    done
    { original $SE/system_ext_sepolicy.cil; echo ";; $MARK"; cat $ST/system_ext_additions.cil; } > $ST/cil.tmp
    cp $ST/cil.tmp $SE/system_ext_sepolicy.cil
    v=$(cat /vendor/etc/selinux/plat_sepolicy_vers.txt)
    cat $SE/system_ext_sepolicy.cil $SE/mapping/$v.cil | sha256sum | cut -d' ' -f1 \
        > $SE/system_ext_sepolicy_and_mapping.sha256
    chmod 0644 $SE/system_ext_*
    if ! compile_as_init $ST/policy; then
        echo "secilc refused the merged policy: SELinux files restored, nothing to reboot for" >&2
        restore_selinux
        exit 1
    fi
    echo "init's compile at boot will succeed ($(wc -c < $ST/policy) bytes)"
    # Load it now, so restorecon can give the new files their types.
    dd if=$ST/policy of=/sys/fs/selinux/load bs=8M 2>/dev/null
    restorecon -RF $E/bin/xkernel_vm_service $E/bin/xkernel_vmctl $E/bin/xkernel_crosvm_launcher \
        $E/bin/xkernel_vm_loader $E/bin/xkernel_crosvm $E/etc/init/xkernel_vm_service.rc \
        $E/etc/xkernel_vm $SE
    if [ -d $MONITOR ]; then restorecon -RF $MONITOR; fi
    if [ -d /data/misc/xkernel_vm ]; then restorecon -RF /data/misc/xkernel_vm; fi
    sync
    ls -Z $E/bin | grep xkernel
    echo "installed: reboot to let init start the service"
    ;;
remove)
    remount system_ext
    for b in $BINS xkernel_crosvm; do rm -f $E/bin/$b; done
    rm -rf $E/etc/init/xkernel_vm_service.rc $E/etc/xkernel_vm $MONITOR
    restore_selinux
    sync
    echo "removed (the overlay stays until enable-verity): reboot"
    ;;
status)
    getprop init.svc.xkernel_vm_service
    ps -AZ -o LABEL,USER,PID,PPID,NAME | grep -E "xkernel_vm_service|crosvm_xkernel" | grep -v grep || true
    lsmod | grep -c xhyper || true
    ls -lZ /dev/xhyper 2>/dev/null || echo "no /dev/xhyper (the driver loads on the first start)"
    $E/bin/xkernel_vmctl status
    ;;
*)
    echo "usage: on-device.sh files|remove|status" >&2
    exit 2
    ;;
esac
