#!/bin/sh
# xkload -- a mixed, randomly paced load for the x-kernel guest, so that the monitor app's two lanes move
# like a real workload instead of sitting at 0 % (idle guest) or at a flat 100 % (while :; do :; done).
#   "可信子系统" = the guest's vCPU time on the host: moved by any guest computation.
#   "X Core"     = time in EL2: moved by guest I/O, interrupts and timers (virtio disk, short sleeps),
#                  hardly by pure computation, which runs without leaving the guest.
# Each round picks one phase at random -- hashing, gzip of random data, disk write+fsync+read-back,
# a burst of 20 ms sleeps -- sizes it at random, sometimes runs it on both vCPUs, then idles a random time.
#
#   xkload start [light|medium|heavy]           run in the background (log /root/xkload.log)
#   xkload stop                                 stop the background run
#   xkload status                               running or not, last rounds
#   xkload run [light|medium|heavy] [SECONDS]   run in the foreground (Ctrl+C stops); no SECONDS = until stopped
# Busybox sh only (the guest has no bash or python). Lives in the guest as /root/xkload.sh with /usr/local/bin/xkload
# pointing at it: t11206-pvm-service guest/add-to-rootfs.sh puts it into a root fs image (the template), and the
# record's install-xkload.sh types it into a running guest through the service console.
D=/root
PIDF=$D/.xkload.pid
LOG=$D/xkload.log
DAT=$D/.xkload.dat

rnd() { echo $(( RANDOM % ($2 - $1 + 1) + $1 )); }

level_params() {
  case "$1" in
    light) IDLE_MIN=2; IDLE_MAX=8; SIZE_MAX=16; PAR_PCT=10 ;;
    heavy) IDLE_MIN=0; IDLE_MAX=1; SIZE_MAX=96; PAR_PCT=60 ;;
    *)     IDLE_MIN=1; IDLE_MAX=4; SIZE_MAX=48; PAR_PCT=30 ;;
  esac
}

ph_hash()  { dd if=/dev/zero bs=1M count=$(rnd 4 $((SIZE_MAX * 4))) 2>/dev/null | sha256sum >/dev/null; }
ph_gzip()  { dd if=/dev/urandom bs=64k count=$(rnd 16 $((SIZE_MAX * 8))) 2>/dev/null | gzip -1 >/dev/null; }
ph_disk()  {
  f=$DAT.${1:-1}   # the two copies of a parallel round get 1 and 2 (a forked shell repeats $RANDOM)
  dd if=/dev/urandom of=$f bs=64k count=$(rnd 16 $((SIZE_MAX * 4))) conv=fsync 2>/dev/null
  sha256sum $f >/dev/null
  rm -f $f; sync
}
ph_ticks() { n=$(rnd 20 200); i=0; while [ $i -lt $n ]; do sleep 0.02; i=$((i + 1)); done; }

one_round() {
  case $(rnd 0 3) in 0) p=ph_hash ;; 1) p=ph_gzip ;; 2) p=ph_disk ;; *) p=ph_ticks ;; esac
  if [ $(rnd 1 100) -le $PAR_PCT ]; then $p 1 & $p 2; wait; w=2; else $p 1; w=1; fi
  idle=$(rnd $IDLE_MIN $IDLE_MAX).$(rnd 0 9)
  echo "$(date +%T) ${p#ph_} x$w, idle ${idle}s"
  sleep $idle
}

loop() {
  level_params "$1"; end=$2; t0=$(date +%s)
  trap 'rm -f $DAT.*; exit 0' INT TERM
  echo "xkload: level ${1:-medium}${end:+, $end s}"
  while :; do
    [ -n "$end" ] && [ $(( $(date +%s) - t0 )) -ge "$end" ] && break
    one_round
  done
  rm -f $DAT.*
}

running() { [ -f $PIDF ] && kill -0 "$(cat $PIDF)" 2>/dev/null; }

case "$1" in
  start)
    if running; then echo "xkload already running (pid $(cat $PIDF))"; exit 0; fi
    nohup sh "$0" _loop "${2:-medium}" > $LOG 2>&1 &
    echo $! > $PIDF
    echo "xkload started (pid $!, level ${2:-medium}); 'xkload status' to watch, 'xkload stop' to end"
    ;;
  stop)
    if ! running; then echo "xkload not running"; rm -f $PIDF; exit 0; fi
    pid=$(cat $PIDF)
    kill "$pid" 2>/dev/null
    pkill -P "$pid" 2>/dev/null
    rm -f $PIDF $DAT.*
    echo "xkload stopped (pid $pid)"
    ;;
  status)
    if running; then echo "xkload running (pid $(cat $PIDF))"; else echo "xkload not running"; fi
    [ -f $LOG ] && tail -n 5 $LOG
    ;;
  run)
    loop "${2:-medium}" "$3"
    ;;
  _loop)
    loop "$2"
    ;;
  *)
    echo "usage: xkload start [light|medium|heavy] | stop | status | run [light|medium|heavy] [SECONDS]"
    ;;
esac
