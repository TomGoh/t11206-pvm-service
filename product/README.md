# 把 x-kernel 虚拟机和 OS之OS 监视器集成进 T11206 镜像

给做平板镜像（PAC 包）的同事。说明要放进源码树的东西、产品配置要加的几行、出包后怎么验收，以及调试平板上的哪些做法不要照搬。

## 一、结果是什么样

刷入镜像后，开机即可使用，不需要任何手动步骤：

- init 在开机完成后启动控制服务 `xkernel_vm_service`。服务这时只注册接口：不加载驱动，不占客户机内存。
- 用户在“X 内核虚拟机”（控制应用）里点启动时，服务请 init 运行 `xkernel_vm_loader` 加载宿主驱动（约 0.2 秒）。第一次启动还会把客户机根文件系统模板复制到 `/data/misc/xkernel_vm/rootfs.img`（1 GiB，只复制一次），之后启动 x-kernel。
- 用户打开“OS之OS监控软件”时，监视器是特权应用，开机时系统已经授予它读取权限。它第一次取数时，服务同样按需加载驱动并开始 EL2 计数；没人取数 15 秒后，服务自动停止计数。

全部内容都在 system_ext 分区，厂商分区不用改。

## 二、放进源码树的东西

| 内容 | 来源 | 装到哪里 | 怎么进镜像 |
|---|---|---|---|
| 控制服务、命令行工具、启动器、驱动加载程序、init 脚本、实例配置、SELinux 策略 | 仓库 `t11206-pvm-service`（本仓库） | `/system_ext/bin`、`/system_ext/etc/init`、`/system_ext/etc/xkernel_vm`、system_ext 策略 | 源码编译，`product.mk` |
| 控制应用“X 内核虚拟机” | 仓库 `t11206-pvm-app` | `/system_ext/app/XKernelVmControl` | 源码编译（平台密钥签名），`product.mk` |
| 宿主驱动模块（7 个 `.ko`，顺序见 `etc/modules.load`） | 宿主驱动的构建，必须与平板内核一起编译（内核一变就要重编） | `/system_ext/etc/xkernel_vm/modules/` | 预编译目录 `modules/`，`product.mk` 复制 |
| 客户机内核 `kernel.bin`、只读盘 `dummy.img`、根文件系统模板 `rootfs.img` | x-kernel 的交付 | `/system_ext/etc/xkernel_vm/` | 预编译目录，`product.mk` 复制 |
| 带虚拟化层后端的 crosvm（静态链接） | crosvm 的交付 | `/system_ext/bin/xkernel_crosvm` | 预编译目录，模板里的 `cc_prebuilt_binary` |
| 监视器“OS之OS监控软件”（`com.kylinos.monitor`） | 仓库 `2030/kylin100/monitor` 分支 `xhyper/el2-xkernel-monitor` 编出的 APK | `/system_ext/priv-app/KylinMonitor/` | 预编译目录，模板里的 `android_app_import`（特权应用） |

步骤：

1. 把 `t11206-pvm-service` 和 `t11206-pvm-app` 两个仓库放进与设备版本一致的 Android 源码树（目录位置不限）。
2. 建一个预编译目录（必须在源码树里，因为里面要放 Android.bp），放入：
   - `modules/` 下的 7 个 `.ko`；
   - `kernel.bin`、`dummy.img`、`rootfs.img`；
   - `xkernel_crosvm`；
   - `KylinMonitor.apk`（已签名）；
   - 把本目录的 `prebuilts.bp.template` 复制成 `Android.bp`。
3. 在产品的 makefile 里设置预编译目录并包含本仓库的 `product.mk`：

   ```make
   XKERNEL_VM_PREBUILTS_DIR := vendor/kylinos/xkernel_vm_prebuilts   # 第 2 步的目录，相对源码树根
   $(call inherit-product, <本仓库路径>/product.mk)                    # 或 include
   ```

   `product.mk` 会加入这些模块：`xkernel_vm_service`、`xkernel_vm.json`、`XKernelVmControl`、`xkernel_crosvm`、`KylinMonitor`，以及只在调试版本中安装的 `xkernel_vmctl`。它还会加入 system_ext 私有策略目录，并复制模块和客户机镜像。

监视器的签名：模板默认保留 APK 自带的签名（`presigned: true`），所以 APK 要先签好。如果想在出包时改用平台密钥重签，把这一行换成 `certificate: "platform"`。两种方式下，它都作为特权应用获得读取权限。

## 三、权限和 SELinux

- 两个权限都由控制应用声明：
  - `CONTROL_VM`（signature）：启停虚拟机、使用客户机控制台。只有平台签名的应用能获得。
  - `READ_VM_STATS`（signature|privileged|development）：只读的监视数据。平台签名的应用和特权应用开机即获得；不需要 privapp 白名单，白名单只检查平台定义的权限（`PermissionManagerServiceImpl.checkPrivilegedPermissionAllowlist`）。
- 策略全部在本仓库的 `sepolicy/private`（system_ext 私有策略），包括服务的域和接口、`/dev/xhyper` 的类型（只有 crosvm 域能打开）、`/proc/xcore_stats` 的标签（只有服务能读写）。策略已经和平板源码树的平台策略一起编译，通过了全部禁止规则检查。
- **展锐分开编译要注意：** 系统和厂商两半分开编译时，厂商一半里的预编译策略（`/odm` 或 `/vendor` 的 `precompiled_sepolicy`）记录了 system_ext 策略的摘要。加了本策略之后，要用新的系统一半重新生成一次厂商一半的预编译策略。否则开机时摘要对不上，init 会现场编译策略：能开机，但每次开机多约 1.5 秒。可以用下面的方法检查：比较 `/system_ext/etc/selinux/system_ext_sepolicy_and_mapping.sha256` 与 `/odm/etc/selinux/precompiled_sepolicy.system_ext_sepolicy_and_mapping.sha256`（或 `/vendor` 下的同名文件），两者应当相同。

## 四、出包后的验收

刷入后，用 adb 依次检查：

```sh
getenforce                                                   # 量产版本应为 Enforcing
getprop init.svc.xkernel_vm_service                          # running
service list | grep com.kylinos.xkernelvm                    # IXKernelVmControl 和 IXKernelVmMonitor 两个接口
ls -lZ /system_ext/bin/xkernel_* /system_ext/etc/xkernel_vm /system_ext/etc/xkernel_vm/modules
dumpsys package com.kylinos.monitor | grep -E "codePath|PRIVILEGED|READ_VM_STATS"   # priv-app，granted=true
dumpsys package com.kylinos.xkernelvm.control | grep codePath
```

然后：

1. 打开“X 内核虚拟机”，点击启动，状态应约 2 秒后变为“已就绪”。第一次启动多出复制根文件系统的时间。
2. 打开“OS之OS监控软件”：“X Core监视器”一栏有 8 条曲线；“可信子系统”一栏在 x-kernel 运行时有数（客户机空闲时约 1%）。
3. 检查有没有 SELinux 拒绝：`logcat -d -b events | grep avc`、`dmesg | grep avc`，重点看 `xkernel_vm_service`、`xkernel_vm_loader`、`crosvm`、`priv_app`（监视器）、`platform_app`（控制应用）。

## 五、调试平板上的做法不要照搬

我们在调试平板上：

- 关闭了分区校验，用 remount 叠加层改写 `/system_ext`（`device/t11206/install.sh`）；
- 在设备上把策略增量编进系统策略；
- 用 `pm grant` 给 adb 装的监视器授权；
- 让平板一直处于宽容模式。

这些只是为了不刷机做测试。在正式镜像里，它们全部由编译过程直接产生，不需要也不应该出现在 PAC 包里。

## 六、还没验证或待定的地方

- **强制模式：** 调试平板一直是宽容模式。策略通过了编译检查，在宽容模式下也没有我们这几个域的拒绝记录，但还没有在强制模式、user 版本上实际跑过。第一次出包请重点做第四节第 3 步。监视器自己读 `/proc/stat` 的“移动运行环境”一栏，在强制模式下可能被平台策略拒绝；这是应用原有的读法，与本次改动无关。
- **根文件系统模板：** 必须是 x-kernel 交付的干净镜像。调试平板 `/data/misc/xkernel_vm/rootfs.img`（1 GiB ext4，sha256 `c5b18fe4…686a`）已经被客户机写过，不能直接当模板。模板会占 system_ext 分区约 1 GiB。
- **虚拟化层：** 本文只管 Android 一侧。启动镜像里的 XHyper 要带 EL2 监视功能和受保护虚拟机支持；它和宿主内核、AVB 密钥属于启动镜像的另一条打包线。
- **参考输入（调试平板 T11206HXH6600007 上验证过的组合）：** crosvm `0ac2449b…c2f2`，`kernel.bin` `d36aa46b…0360`，`dummy.img` `2daeb1f3…3e74`；驱动模块与内核 `5.15.178-android13-8-850930-g0ccc9d9bc3b7` 匹配，模块的校验和见实施记录。
