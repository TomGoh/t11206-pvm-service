#!/system/bin/sh
# Trial (2026-10-16): the protected x-kernel through pvmfw on T11206, with the firmware loaded
# from a temporary directory instead of a pre-boot source. Runs as root (su 0) on the tablet.
#
#   on-device-pvmfw.sh start TAG [KERNEL]   start crosvm detached; log in $D/run-TAG.log
#   on-device-pvmfw.sh stop                 crosvm stop over the socket, then make sure it is gone
#   on-device-pvmfw.sh status
#
# The command line is the control service's direct-backend one (src/direct.rs, the unit test
# command_line_is_the_one_virtmgr_builds) with three changes: --protected-vm-with-firmware
# $D/custom_pvmfw in place of --protected-vm-without-firmware, the AVB-signed kernel, and a third
# disk, the instance disk pvmfw looks for ("vm-instance"). The console goes to the log only (no
# input). CPUS and MEM_MIB override the 2 vCPUs and 256 MiB (+6 MiB swiotlb) of the product config.
set -eu
D=${D:-/data/local/tmp/xkpvm-fw}
CROSVM=${CROSVM:-/data/local/tmp/xhyper-new/crosvm-xhyper}
CPUS=${CPUS:-2}
MEM_MIB=${MEM_MIB:-256}
SOCK=$D/crosvm.sock
NAME=crosvm_xkernel_pvmfw

case "${1:-}" in
start)
    tag=${2:?TAG}
    kernel=${3:-$D/kernel-signed.bin}
    cd $D
    if pgrep -f "$NAME" > /dev/null; then echo "a trial VM is already running" >&2; exit 1; fi
    [ -f instance.img ] || cp instance-empty.img instance.img
    log=$D/run-$tag.log
    {
        echo "PVMFW_TRIAL_BEGIN $tag $(date +%T) boot=$(cat /proc/sys/kernel/random/boot_id)"
        sha256sum "$CROSVM" custom_pvmfw "$kernel" instance.img dummy.img
        echo "cpus=$CPUS mem=$MEM_MIB"
    } > "$log"
    rm -f $SOCK
    setsid "$CROSVM" --extended-status --log-level info --no-syslog run --disable-sandbox \
        --cid 1025 --name $NAME \
        --protected-vm-with-firmware $D/custom_pvmfw \
        --unmap-guest-memory-on-fork "$kernel" \
        --params "panic=-1 console=ttyS0 loglevel=8" --params hostname=xkernel_pvmfw \
        --cpus "num-cores=$CPUS,sve=[auto=true]" \
        --pci "mem=[start=0x2c000000,size=0x2000000],cam=[start=0x2e000000,size=0x1000000]" \
        --no-pmu --mem $((MEM_MIB + 6)) --swiotlb 6 \
        --params coherent_pool=4096 --params rodata=on \
        --serial=type=file,path=/proc/self/fd/1,hardware=serial,num=1 \
        "--serial=type=file,path=/proc/self/fd/1,hardware=virtio-console,num=1,max-queue-sizes=[2,32]" \
        "--serial=type=sink,hardware=virtio-console,num=3,max-queue-sizes=[2,32]" \
        --serial=type=sink,hardware=serial,num=2 \
        "--serial=type=sink,hardware=virtio-console,num=2,max-queue-sizes=[2,32]" \
        --block path=$D/dummy.img,ro=true,lock=false \
        --block path=$D/rootfs.img,ro=false,lock=false \
        --block path=$D/instance.img,ro=false,lock=false \
        --no-usb --socket $SOCK \
        < /dev/null >> "$log" 2>&1 &
    echo "started pid $! log $log"
    ;;
stop)
    if [ -S $SOCK ]; then "$CROSVM" --no-syslog stop $SOCK || true; fi
    for _ in 1 2 3 4 5 6 7 8 9 10; do
        pgrep -f "$NAME" > /dev/null || { echo "stopped"; exit 0; }
        sleep 1
    done
    pkill -9 -f "$NAME" || true
    sleep 1
    if pgrep -f "$NAME" > /dev/null; then echo "still running" >&2; exit 1; fi
    echo "killed"
    ;;
status)
    ps -A -o PID,NAME,ARGS | grep "$NAME" | grep -v grep || echo "no trial VM"
    ;;
*)
    sed -n '2,13p' "$0"
    exit 2
    ;;
esac
