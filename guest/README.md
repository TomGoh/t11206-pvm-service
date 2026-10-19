# 客户机里用的工具

## xkload：x-kernel 里的真实负载模拟

x-kernel 平时没什么事做，监视器“OS之OS监控软件”的两栏几乎一直是 0；而 `while :; do :; done` 这样的死循环只会让一栏平平地顶满。`xkload` 在客户机里跑一种节奏随机的混合负载，让两栏像真实负载一样起伏：

- “可信子系统”一栏是 x-kernel 虚拟处理器线程在宿主上用掉的处理器时间，客户机里的任何计算都会让它动；
- “X Core监视器”一栏是 EL2 时间，主要由客户机的 I/O、中断和定时器带动。纯计算不离开客户机，几乎不进 EL2。

每一轮随机挑一种负载：
- 算哈希；
- 压缩随机数据；
- 写文件并 fsync 再读回（virtio 磁盘）；
- 连续 20 到 200 次 20 毫秒睡眠（定时器唤醒）。

负载的大小随机，有时两个虚拟处理器同时跑；每轮之后随机空闲一段时间。三档强度只改变空闲长短（`light` 2–8 秒、`medium` 1–4 秒、`heavy` 0–1 秒）、负载大小和双路并行的比例。

在控制应用的终端里：

```sh
xkload start [light|medium|heavy]   # 后台运行，日志 /root/xkload.log
xkload status                       # 是否在运行，最近 5 轮
xkload stop
xkload run heavy 60                 # 前台运行 60 秒，Ctrl+C 停
```

客户机只有 busybox（没有 bash、python），脚本只用 busybox `sh`。

## 放进根文件系统模板

```sh
guest/add-to-rootfs.sh rootfs.img
```

这个脚本用 e2fsprogs 的 `debugfs` 直接改镜像文件，不需要 root，也不需要挂载。它放入两样东西：

- `/root/xkload.sh`，属主 root:root，权限 0755；
- 链接 `/usr/local/bin/xkload`，指向 `/root/xkload.sh`。

改完后它会核对模式、属主、链接目标和内容哈希，再跑一遍 `e2fsck -fn`。重复运行是安全的。

模板（`/system_ext/etc/xkernel_vm/rootfs.img`）在虚拟机第一次启动时被复制成可写盘（`/data/misc/xkernel_vm/rootfs.img`）。所以：

- **已有的可写盘不会变。** 要在一台已经用过的设备上用 xkload，需要把它装进正在运行的客户机，或者删掉可写盘、让服务重新从模板复制。
- **做镜像时：** 把 x-kernel 交付的模板放进预编译目录之前，先对它运行一次这个脚本（见 `product/README.md`）。
