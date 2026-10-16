# 在 T11206 调试平板上按产品方式安装

不编译、不刷写镜像，把本服务装成产品镜像里的样子：init 开机拉起服务，第一次启动虚拟机时加载宿主驱动，SELinux 策略随系统扩展分区生效。适用于 userdebug、引导加载程序已解锁（`ro.boot.verifiedbootstate=orange`）的平板；做法是安卓调试版的标准手段——关闭分区校验后，`remount` 给 `/system_ext` 叠一个可写层（`/mnt/scratch`），原分区在下面不动。

## 步骤

```sh
device/t11206/install.sh verity                 # 关闭分区校验，然后重启一次
device/t11206/install.sh files <构建目录>        # 安装，然后重启
device/t11206/install.sh status                 # 查看服务、驱动和虚拟机
```

`<构建目录>` 放本仓库的 Android 构建产物：`xkernel_vm_service`、`xkernel_vmctl`、`xkernel_crosvm_launcher`、`xkernel_vm_loader`。设备上的输入（可用环境变量改）：`CROSVM`（带虚拟化层后端的 crosvm，默认 `/data/local/tmp/xhyper-new/crosvm-xhyper`）、`MODULES_DIR`（宿主驱动的 `.ko`）、`KERNEL` 与 `DUMMY`（客户机内核和只读盘）。应用另行安装（平台签名的 APK，`adb install`）。

## `files` 做什么

| 内容 | 位置 |
|---|---|
| 服务、命令行工具、启动器、加载程序、crosvm | `/system_ext/bin/` |
| init 脚本 | `/system_ext/etc/init/xkernel_vm_service.rc` |
| 实例配置（`etc/xkernel_vm.json`）、客户机内核、只读盘 | `/system_ext/etc/xkernel_vm/` |
| 宿主驱动模块与加载顺序（`etc/modules.load`） | `/system_ext/etc/xkernel_vm/modules/` |
| SELinux | `/system_ext/etc/selinux/`：`system_ext_additions.cil` 接在 `system_ext_sepolicy.cil` 后面；`sepolicy/private` 的三个 contexts 文件的条目接在对应的 `system_ext_*_contexts` 后面；`system_ext_sepolicy_and_mapping.sha256` 改为新内容的摘要 |

可写盘和状态目录仍在 `/data/misc/xkernel_vm`（init 脚本建目录）。

SELinux 的要点：

- 预编译策略（`/odm/etc/selinux/precompiled_sepolicy`）只在各分区的 `*_sepolicy_and_mapping.sha256` 与它记录的一致时使用；摘要文件一变，init 开机时用 `/system/bin/secilc` 现场编译（多约 1.5 秒）。
- `files` 在提示重启之前，用设备上的 `secilc`、按 init 的参数和文件顺序把合并后的策略编译一遍（`on-device.sh` 的 `compile_as_init`）；编译失败就恢复原来的 SELinux 文件并退出，不会留下一个开不了机的策略。编译成功后当场加载这份策略，再用 `restorecon -RF` 按新的 `file_contexts` 给文件打标签（标签写在叠加层文件上，重启后保留）。
- 重复运行不会叠加：合并前先去掉文件里以 `x-kernel VM control service` 那一行开头的旧块。

## 撤销

- `install.sh remove`：去掉我们的文件和 SELinux 块（摘要文件恢复为预编译策略记录的值），然后重启；叠加层还在。
- `install.sh undo`：重新打开分区校验，整个叠加层随之去掉，然后重启。`/data/misc/xkernel_vm` 和应用不受影响。

## `system_ext_additions.cil` 从哪里来

它是 `sepolicy/private/*.te` 编译成 CIL 后、相对平板平台策略多出来的那部分：在平板对应的 Android 源码树里，用构建自己的 m4 命令生成插入与不插入本仓库 `.te` 的两份 system_ext 策略，`checkpolicy -C` 各转一次，取差异（实施记录 `t11206-avf-nofw-impl-20261015` 的 `tools/sepolicy-check/check.sh` 与 `tools/selinux-trial/build-policy.sh`）。`.te` 改动后要重新生成；同一流程还检查全部 neverallow。

## 2026-10-16 平板实测

T11206HXH6600007：按上面三步安装后重启，init 现场编译策略，开机后拉起服务（system 用户、`xkernel_vm_service` 域、可锁定内存不限）；在应用里点“启动”，init 按属性启动加载程序，7 个模块加载、ueventd 按本仓库的 `file_contexts` 把 `/dev/xhyper` 标为 `xhyper_device`、加载程序设为 0666，虚拟机启动、约 2 秒后就绪；运行中 `kill -9` 服务，crosvm 立即结束（启动器的父进程死亡信号与 init 结束进程组），init 随即重启服务，报告“已停止”、回收已确认；全过程没有 SELinux 拒绝记录。重复运行 `files` 后再重启，结果相同。

## 开发配置（`dev/`）

不安装进分区、从 `/data/local/tmp/xkernel-pvm` 临时运行服务时用过的实例配置（2026-10-15/16 的板测，实施记录 `t11206-avf-nofw-impl-20261015`）：

| 文件 | 用途 |
|---|---|
| `xkernel_vm.dev.json` | 经 AVF（需要 Virtualization 模块的配套改动与覆盖挂载）。 |
| `xkernel_vm.direct.dev.json` | 直接后端，磁盘与状态都在 `/data/local/tmp`。 |
| `xkernel_vm.direct.data.dev.json` | 直接后端，可写盘与状态在产品位置 `/data/misc/xkernel_vm`。 |
| `xkernel_vm.direct.selinux.dev.json` | 运行时加载 SELinux 策略的试验：启动器、按需加载驱动、串口就绪信号。 |
| `vm-tool-negative.json` | 反向检查：AVF 的 `vm run --protected` 以未声明的调用方请求同名虚拟机，应被拒绝。 |

## 经 pvmfw 启动的试验（`dev/pvmfw/`）

2026-10-16 的试验：从 `/data/local/tmp` 加载厂商 pvmfw，用 crosvm 的 `--protected-vm-with-firmware` 启动签名的 x-kernel（实施记录 `t11206-pvmfw-trial-20261016`）。结论是客户机能启动，但 pvmfw 没有执行：这一模式下 crosvm 从内核入口启动，未签名的内核同样能启动，实例盘也没有被写入。

| 文件 | 用途 |
|---|---|
| `sign-xkernel.sh` | 用厂商的 SM2 测试密钥给 x-kernel 加 AVB 哈希尾部，签名凑满 72 字节，并用公钥自检；在 aarch64 Linux 里运行。 |
| `make-instance-img.py` | 生成 pvmfw 要找的实例盘：GPT 盘里一个名为 `vm-instance` 的分区，首块写实例头。 |
| `pvmfw-trial.sh` | 主机侧：推送输入（`INPUTS` 指向签名与未签名内核、空实例盘所在的目录，`FW` 指向 pvmfw）；停掉服务的虚拟机；启动、停止试验虚拟机，查看日志。 |
| `on-device-pvmfw.sh` | 平板侧：直接后端的命令行，换成自带固件模式，加上实例盘。 |
