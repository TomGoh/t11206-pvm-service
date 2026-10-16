# t11206-pvm-service

x-kernel 虚拟机的原生控制服务。它是这台设备上 x-kernel 虚拟机的唯一持有者：把 x-kernel 作为**不带客户机固件的受保护虚拟机**创建、启动和停止（首选自己运行 crosvm，也可以经 AVF），并通过 Binder 接口 `IXKernelVmControl` 把这些操作和真实状态提供给控制应用和命令行工具。

## 行为约定

- 服务启动后只注册接口并等待；在有人请求启动之前，不创建 virtmgr、crosvm，也不占用客户机内存。查询状态从不触发启动。
- 虚拟机属于服务，不属于客户端：客户端退出或死亡不影响虚拟机。
- 只有一个实例。运行中再次请求启动会并入当前操作；停止进行中请求启动会被拒绝为“忙”；上一实例未确认回收时拒绝启动。
- “已停止”必须经过回收核实：实例的 crosvm 进程已结束退出（宿主驱动在进程关闭最后一个文件时释放虚拟机）。停止超时或有残留进程时进入“回收失败或状态待核实”，此时再次请求停止会重新核实。
- 每个实例有代次编号，迟到的旧事件被忽略；每次可见变化序号加一，客户端据此丢弃过期快照。
- 服务重启后先核实上一轮留下的进程（`state_dir/instance.json`，含启动标识），再接受启动。
- 虚拟机的串口控制台由服务持有：服务保留最新实例最近 64 KiB 的输出，客户端挂接时先拿到这段历史，之后的输出按“实例号 + 字节偏移”推送，客户端据此去重、发现缺口并重新挂接。输入只发给运行中的实例；客户机不读输入时写入被拒绝为“忙”，不会阻塞。控制台等于客户机里的 shell，所以和启停要求同样的权限，服务也不把控制台内容写进日志。

状态与含义见 `src/machine.rs` 开头的说明，接口见 `aidl/com/kylinos/xkernelvm/`。

## 两种后端

实例配置的 `backend` 决定由谁运行 crosvm，状态机、回收核实、控制台、Binder 接口、命令行工具和应用对两者完全相同：

- `direct`（首选，产品配置 `etc/xkernel_vm.json`）：不改 AVF，也不改厂商分区：添加本服务、控制应用、crosvm 预编译文件和宿主驱动的模块文件即可，全部在 system_ext 分区。服务自己运行一个带虚拟化层后端的 crosvm（`direct.crosvm`），不经 virtmgr，不需要 AVF 声明文件和 AVF 能力属性。命令行与 virtmgr 为这个实例生成的相同（平板实测），只去掉 AVF 独有的设备树覆盖层，日志和故障管道改接空设备，vsock CID 由配置给出并低于 AVF 的分配范围。crosvm 是服务的子进程，进程名同样是 `crosvm_<name>`，在 AVF 的 `crosvm` 安全域里运行，经 `xkernel_crosvm_launcher` 启动：内核在切换安全域时清除父进程死亡信号，启动器进入 `crosvm` 域后重新设置它，再在同一个域里执行 crosvm，所以服务死亡时 crosvm 随之结束（不论服务是否由 init 启动）。停止时先经 crosvm 的控制套接字请求退出，失败或超过 `kill_after_seconds` 仍未退出就强制结束。crosvm 自己的日志写到 `state_dir/crosvm.log`。状态里的 `viaAvf` 表明当前是哪一种。
- `avf`（配置 `etc/xkernel_vm.avf.json`；配置里不写 `backend` 时也按它处理）：经 AVF 创建，需要 Virtualization 模块的配套改动（见下节）。产品改用它时，把 `xkernel_vm.json` 模块的源文件换成这份配置，并把 `protected_vms_without_firmware_xkernel.xml` 加进 `PRODUCT_PACKAGES`。

两者共用的依赖（宿主驱动的加载与设备节点、可写磁盘的位置、SELinux）先按直接后端解决，结论同样用于经 AVF 的路径。

## 宿主驱动按需加载

开机时不加载宿主驱动，设备在有人启动虚拟机之前与原来一样。配置里有 `driver` 时，启动发现 `/dev/xhyper` 不存在（开机后的第一次启动），服务就向属性 `xkernel_vm.load_driver` 写入一个新的请求号；init 脚本据此启动 `xkernel_vm_loader`，它按 `/system_ext/etc/xkernel_vm/modules/modules.load` 的顺序加载同一目录里的模块（已加载的跳过），等到设备节点出现，设为 0666 root root（与 AOSP 的 `/dev/kvm` 相同，由 SELinux 限定只有 crosvm 域能打开），再把 `loaded <请求号>` 或 `failed <请求号>: <原因>` 写到 `xkernel_vm.driver_status`；服务等到这个结果再启动虚拟机，超过 `driver.timeout_seconds` 算失败。服务自己不加载模块：平台策略只把加载模块的权限给专门的域，回答应用请求的服务不应持有它。

模块文件（`.ko`）要与设备的内核一致，随宿主驱动的构建交付，不放在本仓库：做镜像时设置 `XKERNEL_VM_MODULES_DIR` 指向放模块的目录，`product.mk` 把 `etc/modules.load` 列出的模块装到 `/system_ext/etc/xkernel_vm/modules/`（不设置就构建报错）；本仓库带加载顺序 `etc/modules.load`。设备节点的类型 `xhyper_device` 和标签也在本仓库的 system_ext 策略里，因为驱动随本包交付，而不是随厂商分区。若产品改由厂商分区交付驱动（例如 vendor_dlkm），类型、标签和权限就应改由厂商策略和厂商的 ueventd 规则给出。

## 经 AVF 时

服务把实例配置写成 `VirtualMachineRawConfig`（`protectedVm=true`），经 `libvmclient` 在第一次启动时拉起一个 virtmgr，之后的启动都复用它（长期运行的客户端无法按次释放 virtmgr，见 `src/avf.rs` 开头）。默认配置是 2 个虚拟处理器、256 MiB 内存。virtmgr 是否以无固件模式启动由 AVF 决定：产品安装的 `/system_ext/etc/avf/protected_vms_without_firmware_xkernel.xml` 声明虚拟机 `xkernel_vm` 只能由 `/system_ext/bin/xkernel_vm_service` 运行，且虚拟化层要报告 `ro.boot.hypervisor.protected_vm_without_firmware.supported`。这需要 Virtualization 模块的配套改动（分支 `xhyper/protected-without-firmware`）。

## 组成

| 路径 | 内容 |
|---|---|
| `src/machine.rs` | 生命周期状态机（纯逻辑）。 |
| `src/config.rs` | 实例配置与启动前检查（只读文件和属性）。 |
| `src/reclaim.rs` | 回收核实：按进程号加启动时间和进程名查残留。 |
| `src/controller.rs` | 把状态机接到执行线程、定时器和就绪探测上。 |
| `src/avf.rs` | AVF 后端（`libvmclient`）。 |
| `src/direct.rs` | 不经 AVF 的后端：自己运行 crosvm、停止与强制结束、退出码解释。 |
| `src/console.rs` | 串口控制台：每个实例的输入输出管道、输出历史与推送。 |
| `src/service.rs` | `IXKernelVmControl` 的 Binder 实现与调用方校验。 |
| `ctl/main.rs` | 命令行客户端 `xkernel_vmctl status|start|stop|watch|console`。 |
| `etc/` | 实例配置（`xkernel_vm.json` 为直接后端，`xkernel_vm.avf.json` 为经 AVF）、AVF 声明、init 脚本。 |
| `sepolicy/` | system_ext 策略：服务的域、接口、数据目录；启动器转入 `crosvm` 域、crosvm 程序本身的类型；加载程序的域、模块文件、驱动属性；设备节点 `xhyper_device`（只有 crosvm 域能打开）。 |
| `src/driver.rs` | 按需加载宿主驱动：请求、等待结果。 |
| `launcher/` | `xkernel_crosvm_launcher`：进入 `crosvm` 域后重新设置父进程死亡信号，再执行 crosvm。 |
| `loader/` | `xkernel_vm_loader`：按 `modules.load` 加载模块、准备设备节点、报告结果（逻辑在 `loader.rs`，可在开发机测试）。 |
| `product.mk` | 产品集成片段。 |
| `device/t11206/` | 在 T11206 调试平板上不刷机、按产品方式安装（init 拉起、SELinux 生效）的脚本，以及并入系统扩展分区的策略增量 `system_ext_additions.cil`；说明见其 `README.md`。 |
| `host-tests/` | 在开发机上运行纯逻辑模块的单元测试：`cd host-tests && cargo test`；GitHub 在每次推送和拉取请求时运行它，并检查格式和 clippy（`.github/workflows/host-tests.yml`）。服务本身只能在 Android 源码树里编译，不在 CI 中构建。 |

## 构建

放进与设备版本一致的 Android 源码树后：`m xkernel_vm_service xkernel_vmctl`；应用在独立仓库 `t11206-pvm-app`。

## 已知限制

- 经 AVF 时，能力属性目前没有来源：虚拟化层的资源管理器还不计算它，开发验证时由 root 设置一次。直接后端不用它。
- 经 AVF 时，AVF 对受保护虚拟机的磁盘镜像有 SELinux 标签检查；放在 `/data/misc/xkernel_vm` 的可写 rootfs 不在允许列表内，正式布局待定。
- 服务以 `system` 用户运行，没有特权；宿主驱动按可锁定内存上限固定客户机内存，所以 init 脚本把上限设为不限（`rlimit memlock`）。经 AVF 的方式在非 root 下还没有验证过。加载程序以 root 运行，只有加载模块和改设备节点属主的特权。
- SELinux：策略经平台 neverallow 编译检查；2026-10-16 在平板上运行时加载平板策略加本策略，宽容模式下服务和 crosvm 没有一次访问被拒绝（启动器与按需加载另见实施记录）。还没有在强制模式、也没有由 init 启动的服务上验证。
- 就绪：产品配置以串口输出里出现客户机的 shell 提示符 `kylin-x:~#` 作为就绪信号（`readiness.console_marker`），客户机不需要任何改动；提示符变了要同步改配置。也可以用 vsock 端口（`readiness.vsock_port`），但需要客户机监听，且在 SELinux 下只有经 AVF 时能用（平台策略只允许 AVF 的管理进程用 vsock）。没有配置时实例停在“运行中，服务未就绪”。
- 无固件模式不经固件认证镜像，也不传递 DICE 身份；宿主复位时不清理该虚拟机内存（与带固件路径不同）。
- 直接后端：crosvm 二进制要由产品另行安装（示例配置假定在 `/system_ext/bin/xkernel_crosvm`），本仓库不带这个预编译文件；平台策略只允许 AVF 的 `crosvm` 域打开 `/dev/vhost-vsock` 和虚拟机管理设备，所以 crosvm 由服务转入这个域运行；服务的就绪握手不能用服务自己发起的 vsock 连接（平台策略只允许 AVF 的管理进程、adbd 和 traced 用 vsock）；配置里没有 `launcher` 时，crosvm 直接由服务启动，转入 `crosvm` 域后父进程死亡信号被清除，服务被杀死后只有 init 结束进程组这一层保护；这个 crosvm 版本在 vCPU 出错后也以 33 退出，结束原因会被记成“客户机关机”。
