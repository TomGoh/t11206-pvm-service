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

- `direct`（首选，产品配置 `etc/xkernel_vm.json`）：不改 AVF：添加本服务、控制应用和 crosvm 预编译文件即可，另需厂商侧给 `/dev/xhyper` 配标签和权限（`vendor/`）。服务自己运行一个带虚拟化层后端的 crosvm（`direct.crosvm`），不经 virtmgr，不需要 AVF 声明文件和 AVF 能力属性。命令行与 virtmgr 为这个实例生成的相同（平板实测），只去掉 AVF 独有的设备树覆盖层，日志和故障管道改接空设备，vsock CID 由配置给出并低于 AVF 的分配范围。crosvm 是服务的子进程，进程名同样是 `crosvm_<name>`，在 AVF 的 `crosvm` 安全域里运行；服务死亡时由 init 结束服务的整个进程组，crosvm 随之结束（父进程死亡信号会被安全域切换清除，只在不切换域时起作用）。停止时先经 crosvm 的控制套接字请求退出，失败或超过 `kill_after_seconds` 仍未退出就强制结束。crosvm 自己的日志写到 `state_dir/crosvm.log`。状态里的 `viaAvf` 表明当前是哪一种。
- `avf`（配置 `etc/xkernel_vm.avf.json`；配置里不写 `backend` 时也按它处理）：经 AVF 创建，需要 Virtualization 模块的配套改动（见下节）。产品改用它时，把 `xkernel_vm.json` 模块的源文件换成这份配置，并把 `protected_vms_without_firmware_xkernel.xml` 加进 `PRODUCT_PACKAGES`。

两者共用的依赖（`/dev/xhyper` 的厂商侧标签与权限、可写磁盘的位置、SELinux）先按直接后端解决，结论同样用于经 AVF 的路径。

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
| `sepolicy/` | system_ext 策略：服务的域、接口、数据目录，直接后端的 crosvm 可执行文件类型及其转入 `crosvm` 域的规则。 |
| `vendor/` | 厂商分区要带的 `/dev/xhyper` 类型、标签（`sepolicy/`）和权限（`ueventd.rc`），说明见 `vendor/README.md`。 |
| `product.mk` | 产品集成片段。 |
| `host-tests/` | 在开发机上运行纯逻辑模块的单元测试：`cd host-tests && cargo test`；GitHub 在每次推送和拉取请求时运行它，并检查格式和 clippy（`.github/workflows/host-tests.yml`）。服务本身只能在 Android 源码树里编译，不在 CI 中构建。 |

## 构建

放进与设备版本一致的 Android 源码树后：`m xkernel_vm_service xkernel_vmctl`；应用在独立仓库 `t11206-pvm-app`。

## 已知限制

- 经 AVF 时，能力属性目前没有来源：虚拟化层的资源管理器还不计算它，开发验证时由 root 设置一次。直接后端不用它。
- 经 AVF 时，AVF 对受保护虚拟机的磁盘镜像有 SELinux 标签检查；放在 `/data/misc/xkernel_vm` 的可写 rootfs 不在允许列表内，正式布局待定。
- 服务以 `system` 用户运行，没有特权；宿主驱动按可锁定内存上限固定客户机内存，所以 init 脚本把上限设为不限（`rlimit memlock`）。经 AVF 的方式在非 root 下还没有验证过。
- SELinux：策略经平台 neverallow 编译检查；2026-10-16 在平板上运行时加载平板策略加本策略和 `vendor/` 的类型，宽容模式下服务和 crosvm 没有一次访问被拒绝。还没有在强制模式、也没有由 init 启动的服务上验证。
- 没有就绪握手配置时实例不会显示为“服务可用”；x-kernel 侧的握手端口待定。
- 无固件模式不经固件认证镜像，也不传递 DICE 身份；宿主复位时不清理该虚拟机内存（与带固件路径不同）。
- 直接后端：crosvm 二进制要由产品另行安装（示例配置假定在 `/system_ext/bin/xkernel_crosvm`），本仓库不带这个预编译文件；平台策略只允许 AVF 的 `crosvm` 域打开 `/dev/vhost-vsock` 和虚拟机管理设备，所以 crosvm 由服务转入这个域运行；服务的就绪握手不能用服务自己发起的 vsock 连接（平台策略只允许 AVF 的管理进程、adbd 和 traced 用 vsock）；服务不经 init 启动又被杀死时 crosvm 会留下，重启后的服务只报告“回收待核实”，不会替它结束进程；这个 crosvm 版本在 vCPU 出错后也以 33 退出，结束原因会被记成“客户机关机”。
