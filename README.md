# t11206-pvm-service

x-kernel 虚拟机的原生控制服务。它是这台设备上 x-kernel 虚拟机的唯一持有者：通过 AVF 把 x-kernel 作为**不带客户机固件的受保护虚拟机**创建、启动和停止，并通过 Binder 接口 `IXKernelVmControl` 把这些操作和真实状态提供给控制应用和命令行工具。

## 行为约定

- 服务启动后只注册接口并等待；在有人请求启动之前，不创建 virtmgr、crosvm，也不占用客户机内存。查询状态从不触发启动。
- 虚拟机属于服务，不属于客户端：客户端退出或死亡不影响虚拟机。
- 只有一个实例。运行中再次请求启动会并入当前操作；停止进行中请求启动会被拒绝为“忙”；上一实例未确认回收时拒绝启动。
- “已停止”必须经过回收核实：实例的 crosvm 进程已结束退出（宿主驱动在进程关闭最后一个文件时释放虚拟机）。停止超时或有残留进程时进入“回收失败或状态待核实”，此时再次请求停止会重新核实。
- 每个实例有代次编号，迟到的旧事件被忽略；每次可见变化序号加一，客户端据此丢弃过期快照。
- 服务重启后先核实上一轮留下的进程（`state_dir/instance.json`，含启动标识），再接受启动。

状态与含义见 `src/machine.rs` 开头的说明，接口见 `aidl/com/kylinos/xkernelvm/`。

## 与 AVF 的关系

服务把实例配置写成 `VirtualMachineRawConfig`（`protectedVm=true`），经 `libvmclient` 每次启动拉起一个 virtmgr。virtmgr 是否以无固件模式启动由 AVF 决定：本仓库安装的 `/system_ext/etc/avf/protected_vms_without_firmware_xkernel.xml` 声明虚拟机 `xkernel_vm` 只能由 `/system_ext/bin/xkernel_vm_service` 运行，且虚拟化层要报告 `ro.boot.hypervisor.protected_vm_without_firmware.supported`。这需要 Virtualization 模块的配套改动（分支 `xhyper/protected-without-firmware`）。

## 组成

| 路径 | 内容 |
|---|---|
| `src/machine.rs` | 生命周期状态机（纯逻辑）。 |
| `src/config.rs` | 实例配置与启动前检查（只读文件和属性）。 |
| `src/reclaim.rs` | 回收核实：按进程号加启动时间和进程名查残留。 |
| `src/controller.rs` | 把状态机接到执行线程、定时器和就绪探测上。 |
| `src/avf.rs` | AVF 后端（`libvmclient`）。 |
| `src/service.rs` | `IXKernelVmControl` 的 Binder 实现与调用方校验。 |
| `ctl/main.rs` | 命令行客户端 `xkernel_vmctl status|start|stop|watch`。 |
| `etc/` | 实例配置、AVF 声明、init 脚本。 |
| `sepolicy/` | system_ext 策略；`sepolicy/vendor/README.md` 说明设备节点的厂商侧标签。 |
| `product.mk` | 产品集成片段。 |
| `host-tests/` | 在开发机上运行纯逻辑模块的单元测试：`cd host-tests && cargo test`。 |

## 构建

放进与设备版本一致的 Android 源码树后：`m xkernel_vm_service xkernel_vmctl`；应用在独立仓库 `t11206-pvm-app`。

## 已知限制

- 能力属性目前没有来源：虚拟化层的资源管理器还不计算它，开发验证时由 root 设置一次。
- AVF 对受保护虚拟机的磁盘镜像有 SELinux 标签检查；`etc/xkernel_vm.json` 里放在 `/data/misc/xkernel_vm` 的可写 rootfs 不在允许列表内，正式布局待定。
- `/dev/xhyper` 目前只有 root 可访问，服务以 root 运行；SELinux 策略尚未在 Enforcing 下验证。
- 没有就绪握手配置时实例不会显示为“服务可用”；x-kernel 侧的握手端口待定。
- 无固件模式不经固件认证镜像，也不传递 DICE 身份；宿主复位时不清理该虚拟机内存（与带固件路径不同）。
