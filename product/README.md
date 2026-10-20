# 把 x-kernel 虚拟机和 OS之OS 监视器集成进 T11206 镜像

给做平板镜像（PAC 包）的同事。本文说明两种交付方式各要放进源码树的东西、产品配置要加的几行、监视器怎么按 Kylin 应用的惯例导入、出包后怎么验收，以及调试平板上的哪些做法不要照搬。

## 一、结果是什么样

刷入镜像后，开机即可使用，不需要任何手动步骤：

- init 在开机完成后启动控制服务 `xkernel_vm_service`。服务这时只注册接口：不加载驱动，不占客户机内存。
- 用户在“X 内核虚拟机”（控制应用）里点启动时，服务请 init 运行 `xkernel_vm_loader` 加载宿主驱动（约 0.2 秒）。第一次启动还会把客户机根文件系统模板复制到 `/data/misc/xkernel_vm/rootfs.img`（1 GiB，只复制一次），之后启动 x-kernel。
- 用户打开“OS之OS监控软件”时，监视器是平台签名的特权应用，开机时系统已经授予它读取权限。它第一次取数时，服务同样按需加载驱动并开始 EL2 计数；没人取数 15 秒后，服务自动停止计数。

服务、控制应用和它们的文件都在 system_ext 分区；监视器和其他 Kylin 应用一样在 product 分区。厂商分区不用改。

## 二、两种交付方式

服务和控制应用可以按下面任一种方式交付，内容和装到设备上的结果相同：

1. **源码交付（第三节）：** 把本仓库和 `t11206-pvm-app` 两个仓库放进源码树，由镜像构建从源码编译，引入本仓库的 `product.mk`。
2. **二进制交付（第四节）：** 我们在与产品同一安卓基线的源码树里编好，交一个自带全部文件的目录，镜像构建把它当作预编译文件导入，引入目录里的 `product.mk`。

怎么选：

- 能把两个仓库放进源码树，就用源码交付。它没有版本对齐的问题，签名也由镜像构建负责。
- 只收现成文件时用二进制交付。服务在运行时依赖平台内部库 `libbinder_rpc_unstable`，它不保证在安卓版本之间兼容，所以我们编译用的源码树必须和产品发版的安卓基线一致；产品换基线后，要用新基线重新编一次、重新交付。
- 两种方式的模块同名，不能同时用：同一棵树里既有两个源码仓库、又有二进制交付目录时，构建会报模块重复。

两种方式共同的前提：宿主驱动的模块（`.ko`）必须和发版的内核一起编译，内核版本标记完全一致；内核一变就要重编。

监视器在两种方式下都按 Kylin 应用的惯例单独导入，见第五节。

## 三、源码交付

| 内容 | 来源 | 装到哪里 | 怎么进镜像 |
|---|---|---|---|
| 控制服务、命令行工具、启动器、驱动加载程序、init 脚本、实例配置、SELinux 策略 | 仓库 `t11206-pvm-service`（本仓库） | `/system_ext/bin`、`/system_ext/etc/init`、`/system_ext/etc/xkernel_vm`、system_ext 策略 | 源码编译，`product.mk` |
| 控制应用“X 内核虚拟机” | 仓库 `t11206-pvm-app` | `/system_ext/app/XKernelVmControl` | 源码编译（平台密钥签名），`product.mk` |
| 宿主驱动模块（7 个 `.ko`，顺序见 `etc/modules.load`） | 宿主驱动的构建，必须与平板内核一起编译 | `/system_ext/etc/xkernel_vm/modules/` | 预编译目录 `modules/`，`product.mk` 复制 |
| 客户机内核 `kernel.bin`、只读盘 `dummy.img`、根文件系统模板 `rootfs.img` | x-kernel 的交付 | `/system_ext/etc/xkernel_vm/` | 预编译目录，`product.mk` 复制 |
| crosvm | 平台自己的 crosvm（com.android.virt），由开启 `xhyper` 功能的 external/crosvm 编出（kylin100-pad-dev，Gerrit 324346–324349） | `/apex/com.android.virt/bin/crosvm` | 随系统镜像的 com.android.virt，本包不带 |

步骤：

1. 把 `t11206-pvm-service` 和 `t11206-pvm-app` 两个仓库放进与设备版本一致的 Android 源码树（目录位置不限）。
2. 在源码树里建一个预编译目录，放入：
   - `modules/` 下的 7 个 `.ko`；
   - `kernel.bin`、`dummy.img`、`rootfs.img`；放入 `rootfs.img` 前先运行 `guest/add-to-rootfs.sh rootfs.img`，把负载模拟工具 `xkload` 放进模板（见 `guest/README.md`）；
3. 在产品的 makefile 里设置预编译目录并包含本仓库的 `product.mk`：

   ```make
   XKERNEL_VM_PREBUILTS_DIR := vendor/kylinos/xkernel_vm_prebuilts   # 第 2 步的目录，相对源码树根
   $(call inherit-product, <本仓库路径>/product.mk)                    # 或 include
   ```

   `product.mk` 会加入这些模块：`xkernel_vm_service`、`xkernel_vm.json`、`XKernelVmControl`，以及只在调试版本中安装的 `xkernel_vmctl`。它还会加入 system_ext 私有策略目录，并复制模块和客户机镜像。

## 四、二进制交付

### 我们这边：生成交付目录

1. 在与产品发版同一安卓基线的源码树里，按源码交付的第 1 步放入两个仓库，只编译这几个模块：

   ```sh
   m xkernel_vm_service xkernel_vmctl XKernelVmControl
   ```

   T11206 的系统一半用 `ussi_arm64_full`，产物在 `out_system/target/product/ussi_arm64`。
2. 准备和源码交付第 2 步相同的预编译文件：`modules/` 下的 7 个 `.ko`、`kernel.bin`、`dummy.img`、`rootfs.img`（已放入 `xkload`）。这里不需要 `Android.bp`。
3. 运行本目录的 `binary/make-drop.sh`：

   ```sh
   product/binary/make-drop.sh --product-out out_system/target/product/ussi_arm64 \
       --prebuilts <第 2 步的目录> --out <交付目录> --app-commit <t11206-pvm-app 的提交>
   ```

   脚本把编好的程序和控制应用、本仓库的 init 脚本、实例配置、SELinux 策略源文件，以及预编译文件放进交付目录，生成 `Android.bp` 和 `product.mk`，并写出 `MANIFEST.txt`：每个文件的 sha256、来源提交、安卓基线（默认取系统一半的 `ro.system.build.fingerprint`），以及各程序依赖的共享库。它会拒绝以下情况：输出目录不为空，缺少 `modules.load` 列出的模块，`modules/` 里有多余文件，程序不是 AArch64 ELF，crosvm 不是静态链接，程序实际依赖的共享库和 `Android.bp` 声明的不一致。

### 同事那边：导入交付目录

1. 把交付目录放进源码树，位置不限，比如 `vendor/kylinos/xkernel_vm`。确认树里没有 `t11206-pvm-service` 和 `t11206-pvm-app` 两个仓库。
2. 在产品的 makefile 里加一行：

   ```make
   $(call inherit-product, vendor/kylinos/xkernel_vm/product.mk)
   ```

3. 完整编译出包。控制应用由构建用平台密钥重新签名（`certificate: "platform"`），交付目录里的 APK 用什么签名都可以。

交付目录的内容：

| 路径 | 内容 | 装到哪里 |
|---|---|---|
| `bin/` | `xkernel_vm_service`、`xkernel_crosvm_launcher`、`xkernel_vm_loader`、`xkernel_vmctl`（只进调试版本） | `/system_ext/bin` |
| `app/XKernelVmControl.apk` | 控制应用 | `/system_ext/app/XKernelVmControl` |
| `etc/` | init 脚本、实例配置 `xkernel_vm.json`、`modules.load` | `/system_ext/etc/init`、`/system_ext/etc/xkernel_vm` |
| `modules/` | 宿主驱动的 7 个模块 | `/system_ext/etc/xkernel_vm/modules` |
| `kernel.bin`、`dummy.img`、`rootfs.img` | x-kernel 客户机内核和磁盘 | `/system_ext/etc/xkernel_vm` |
| `sepolicy/private/` | SELinux 策略源文件，由镜像构建编译 | system_ext 策略 |
| `Android.bp`、`product.mk`、`MANIFEST.txt` | 预编译模块、产品配置、清单 | 不安装 |

## 五、监视器：按 Kylin 应用的惯例导入

平板的源码树里没有监视器。它和其他 Kylin 应用一样，作为现成的 APK 导入：

1. 建目录 `vendor/kylinos/apps/kylinos_apps/KylinosMonitor/`，放入：
   - `KylinosMonitor.apk`：监视器仓库（`2030/kylin100/monitor`，分支 `xhyper/t11206-xkernel-monitor`）用 Gradle 编出的 APK，未签名或用任何密钥签名都可以；
   - `Android.bp`：复制本目录的 `monitor/Android.bp.template`；
   - 按惯例附一个 `ChangeLog.txt`。
2. 在 `vendor/kylinos/apps/kylinos_apps/device.mk` 的 `PRODUCT_PACKAGES` 里加 `KylinosMonitor`。

它装在 `/product/priv-app/KylinosMonitor`，由构建用平台密钥签名，是特权应用，开机即获得 `READ_VM_STATS`，不需要权限白名单。监视器只在设备上有本服务的监视接口时显示真实的 EL2 和 x-kernel 占用；没有这个接口的设备（例如 pKVM 的 N80/N90）保持原来的显示。

## 六、权限和 SELinux

- 两个权限都由控制应用声明：
  - `CONTROL_VM`（signature）：启停虚拟机、使用客户机控制台。只有平台签名的应用能获得。
  - `READ_VM_STATS`（signature|privileged|development）：只读的监视数据。平台签名的应用和特权应用开机即获得；不需要 privapp 白名单，白名单只检查平台定义的权限（`PermissionManagerServiceImpl.checkPrivilegedPermissionAllowlist`）。
- 策略全部在本仓库的 `sepolicy/private`（system_ext 私有策略），包括服务的域和接口、`/dev/xhyper` 的类型（只有 crosvm 域能打开）、`/proc/xcore_stats` 的标签（只有服务能读写）。两种交付方式交的都是策略源文件，由镜像构建和平台策略一起编译，并检查全部禁止规则。
- 控制应用和监视器都是平台签名，运行在 `platform_app` 域：策略允许它调用服务、查找两个接口。
- **展锐分开编译要注意：** 系统和厂商两半分开编译时，厂商一半里的预编译策略（`/odm` 或 `/vendor` 的 `precompiled_sepolicy`）记录了 system_ext 策略的摘要。加了本策略之后，要用新的系统一半重新生成一次厂商一半的预编译策略。否则开机时摘要对不上，init 会现场编译策略：能开机，但每次开机多约 1.5 秒。可以用下面的方法检查：比较 `/system_ext/etc/selinux/system_ext_sepolicy_and_mapping.sha256` 与 `/odm/etc/selinux/precompiled_sepolicy.system_ext_sepolicy_and_mapping.sha256`（或 `/vendor` 下的同名文件），两者应当相同。

## 七、出包后的验收

刷入后，用 adb 依次检查：

```sh
getenforce                                                   # 量产版本应为 Enforcing
getprop init.svc.xkernel_vm_service                          # running
service list | grep com.kylinos.xkernelvm                    # IXKernelVmControl 和 IXKernelVmMonitor 两个接口
ls -lZ /system_ext/bin/xkernel_* /system_ext/etc/xkernel_vm /system_ext/etc/xkernel_vm/modules
dumpsys package com.kylinos.monitor | grep -E "codePath|PRIVILEGED|READ_VM_STATS"   # /product/priv-app/KylinosMonitor，granted=true
dumpsys package com.kylinos.xkernelvm.control | grep codePath                       # /system_ext/app/XKernelVmControl
```

然后：

1. 打开“X 内核虚拟机”，点击启动，状态应约 2 秒后变为“已就绪”。第一次启动多出复制根文件系统的时间。
2. 打开“OS之OS监控软件”：“轻量级虚拟化监视器”一栏有 8 条曲线；“可信子系统”一栏在 x-kernel 运行时有数（客户机空闲时约 1%）。
3. 检查有没有 SELinux 拒绝：`logcat -d -b events | grep avc`、`dmesg | grep avc`，重点看 `xkernel_vm_service`、`xkernel_vm_loader`、`crosvm`、`platform_app`（控制应用和监视器）。

## 八、调试平板上的做法不要照搬

我们在调试平板上：

- 关闭了分区校验，用 remount 叠加层改写系统分区（`device/t11206/install.sh`）；
- 在设备上把策略增量编进系统策略；
- 用 `pm grant` 给 adb 装的监视器授权；
- 让平板一直处于宽容模式。

这些只是为了不刷机做测试。在正式镜像里，它们全部由编译过程直接产生，不需要也不应该出现在 PAC 包里。

## 九、还没验证或待定的地方

- **强制模式：** 调试平板一直是宽容模式。策略通过了编译检查，在宽容模式下也没有我们这几个域的拒绝记录，但还没有在强制模式、user 版本上实际跑过。第一次出包请重点做第七节第 3 步。监视器自己读 `/proc/stat` 的“移动运行环境”一栏，在强制模式下可能被平台策略拒绝；这是应用原有的读法，与本次改动无关。
- **根文件系统模板：** 必须是 x-kernel 交付的干净镜像。调试平板 `/data/misc/xkernel_vm/rootfs.img`（1 GiB ext4，sha256 `c5b18fe4…686a`）已经被客户机写过，不能直接当模板。模板会占 system_ext 分区约 1 GiB。调试平板上的模板已经加了 `xkload`（`968b0bb9…`）。
- **虚拟化层：** 本文只管 Android 一侧。启动镜像里的 XHyper 要带 EL2 监视功能和受保护虚拟机支持；它和宿主内核、AVB 密钥属于启动镜像的另一条打包线。
- **参考输入（调试平板 T11206HXH6600007 上验证过的组合）：** crosvm `0ac2449b…c2f2`，`kernel.bin` `d36aa46b…0360`，`dummy.img` `2daeb1f3…3e74`；驱动模块与内核 `5.15.178-android13-8-850930-g0ccc9d9bc3b7` 匹配，模块的校验和见实施记录。
