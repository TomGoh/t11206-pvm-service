# t11206-pvm-service

x-kernel 虚拟机的原生控制服务。它是这台设备上 x-kernel 虚拟机的唯一持有者：把 x-kernel 作为**不带客户机固件的受保护虚拟机**创建、启动和停止（首选自己运行 crosvm，也可以经 AVF），并通过 Binder 接口 `IXKernelVmControl` 把这些操作和真实状态提供给控制应用和命令行工具；另一个只读接口 `IXKernelVmMonitor` 给监视器应用提供虚拟化层每核的 EL2 周期数和 x-kernel 虚拟机的处理器时间。

## 行为约定

- 服务启动时先核实上一实例；没有残留实例时异步卸载可能遗留的驱动；接口可在清理期间注册并查询“停止中”，此时请求启动返回“忙”；在有人请求启动之前，不创建 virtmgr、crosvm，也不占用客户机内存。查询状态从不触发启动。
- 虚拟机属于服务，不属于客户端：客户端退出或死亡不影响虚拟机。
- 只有一个实例。运行中再次请求启动会并入当前操作；停止进行中请求启动会被拒绝为“忙”；上一实例未确认回收时拒绝启动。
- “已停止”必须经过回收核实和驱动卸载：实例的 crosvm 进程已结束退出（宿主驱动在进程关闭最后一个文件时释放虚拟机），随后服务停止自己开启的 EL2 计数并等待加载程序卸载。整个过程保持“停止中”，此时启动仍返回“忙”。卸载失败或超时会记录原因并进入“已停止”，避免开关卡住；下一实例结束时重试。停止超时或有残留进程时进入“回收失败或状态待核实”，此时再次请求停止会重新核实，回收未确认时绝不卸载。客户机自行退出、崩溃和启动失败同样经过卸载；卸载成功后，崩溃和启动失败仍保持原来的“失败”状态和原因。
- 每个实例有代次编号，迟到的旧事件被忽略；每次可见变化序号加一，客户端据此丢弃过期快照。
- 服务重启后先核实上一轮留下的进程（`state_dir/instance.json`，含启动标识），也扫描进程名。启动时和后续扫描发现的进程号与启动时间都会加入跟踪记录并保存，避免进程在阻塞退出时清空命令行后被误判为消失。发现残留时保持“回收失败或状态待核实”，由串行执行器按回收窗口持续核实；残留仍在时拒绝启动且不卸载，消失后自动确认回收、清理驱动并进入“已停止”，不依赖客户端再次请求停止。等待期间的重复停止复用同一条检查流程，不增加检查器。
- “失败”是接口原有的关闭终态：崩溃或启动失败只有在回收确认和驱动清理完成后才进入这个状态，并保留原因；此后可以直接重新请求启动。卸载失败则仍按上述约定进入“已停止”并记录原因。
- 虚拟机的串口控制台由服务持有：服务保留最新实例最近 64 KiB 的输出，客户端挂接时先拿到这段历史，之后的输出按“实例号 + 字节偏移”推送，客户端据此去重、发现缺口并重新挂接。输入只发给运行中的实例；客户机不读输入时写入被拒绝为“忙”，不会阻塞。控制台等于客户机里的 shell，所以和启停要求同样的权限，服务也不把控制台内容写进日志。

状态与含义见 `src/machine.rs` 开头的说明，接口见 `aidl/com/kylinos/xkernelvm/`。

## 两种后端

实例配置的 `backend` 决定由谁运行 crosvm，状态机、回收核实、控制台、Binder 接口、命令行工具和应用对两者完全相同：

- `direct`（首选，产品配置 `etc/xkernel_vm.json`）：不改 AVF，也不改厂商分区：添加本服务、控制应用、crosvm 预编译文件和宿主驱动的模块文件即可，全部在 system_ext 分区。服务自己运行一个带虚拟化层后端的 crosvm（`direct.crosvm`），不经 virtmgr，不需要 AVF 声明文件和 AVF 能力属性。命令行与 virtmgr 为这个实例生成的相同（平板实测），只去掉 AVF 独有的设备树覆盖层，日志和故障管道改接空设备，vsock CID 由配置给出并低于 AVF 的分配范围。crosvm 是服务的子进程，进程名同样是 `crosvm_<name>`，在 AVF 的 `crosvm` 安全域里运行，经 `xkernel_crosvm_launcher` 启动：内核在切换安全域时清除父进程死亡信号，启动器进入 `crosvm` 域后重新设置它，再在同一个域里执行 crosvm，所以服务死亡时 crosvm 随之结束（不论服务是否由 init 启动）。停止时先经 crosvm 的控制套接字请求退出，失败或超过 `kill_after_seconds` 仍未退出就强制结束。crosvm 自己的日志写到 `state_dir/crosvm.log`。状态里的 `viaAvf` 表明当前是哪一种。
- `avf`（配置 `etc/xkernel_vm.avf.json`；配置里不写 `backend` 时也按它处理）：经 AVF 创建，需要 Virtualization 模块的配套改动（见下节）。产品改用它时，把 `xkernel_vm.json` 模块的源文件换成这份配置，并把 `protected_vms_without_firmware_xkernel.xml` 加进 `PRODUCT_PACKAGES`。

两者共用的依赖（宿主驱动的加载与设备节点、可写磁盘的位置、SELinux）先按直接后端解决，结论同样用于经 AVF 的路径。

## 宿主驱动按需加载

开机时不加载宿主驱动。配置里有 `driver` 时，只有虚拟机启动路径会请求加载，每次启动都检查完整模块列表，避免部分卸载失败后设备节点仍在而漏载模块；打开监视软件、查询状态和读取监视接口都不加载或重新准备模块。服务向现有属性 `xkernel_vm.load_driver` 写入 `load:<服务进程号>.<序号>`，init 已有的通配触发器运行 `xkernel_vm_loader`，不需要修改 init 配置。加载程序按 `/system_ext/etc/xkernel_vm/modules/modules.load` 的顺序加载同目录模块，跳过 `/sys/module/<模块名>` 已存在的模块，等待 `/dev/xhyper`，保持原有的设备节点和计数文件权限准备逻辑。加载结果为 `loaded <完整请求值>` 或 `failed <完整请求值>: <原因>`；服务只接受当前请求的结果，超过 `driver.timeout_seconds` 算失败。程序在操作前捕获完整请求，不会拿另一个操作的请求号代答。

每个实例结束且回收确认后，服务先禁止监视读取重新开启计数，停止自己开启的计数，然后通过同一个属性写入 `unload:<服务进程号>.<序号>`；结果为 `unloaded <完整请求值>` 或 `failed <完整请求值>: <原因>`。加载和卸载共用串行锁；发下一请求前还会等待 `init.svc.xkernel_vm_loader` 不再是运行中，避免上次结果已经发布但 oneshot 进程尚未退出时丢失新触发器。这个等待也受同一个超时限制；即使设备节点存在，新启动也必须先等上一次卸载真正结束。

卸载程序只检查 `/sys/module/<模块名>` 是否存在，再按 `modules.load` 的逆序执行 `delete_module(模块名, O_NONBLOCK)`：`xhyper_ioeventfd`、`xhyper_irqfd`、`xhyper_vcpu`、`xhyper_rsc_mgr`、`xhyper_platform_hooks`、`xhyper`、`xhyper_hypercall`。最后两个模块的忙错误符合预期：`xhyper` 是永久模块，`xhyper_hypercall` 被它引用，所以这两个保留到重启。其他五个模块必须消失，`/dev/xhyper` 和 `/proc/xcore_stats` 也必须消失，才报告卸载成功。一次失败不会阻止继续尝试其余模块。不读取全局模块列表，也不读取模块目录下的引用计数等文件；现有加载程序的模块能力和节点属性权限已经够用，没有修改策略、权限、init 或产品配置。

服务自己不持有模块加载或卸载能力。服务重新启动且没有残留虚拟机时，也走同样的卸载流程，使关闭开关的状态恢复为没有启动过虚拟机的状态；如果仍有实例残留，则保留驱动并阻止新启动，由一个自动检查流程持续核实，残留消失后自行进入卸载。每次核实只占用一个有界回收窗口（至少 200 毫秒），随后排队下一次核实；代次和阶段检查防止过期任务影响新实例或重复卸载。卸载失败时监视接口仍被禁用，直到下次启动重新允许。
模块文件（`.ko`）要与设备的内核一致，随宿主驱动的构建交付，不放在本仓库：做镜像时把它们放进预编译目录 `XKERNEL_VM_PREBUILTS_DIR` 的 `modules/` 下，`product.mk` 把 `etc/modules.load` 列出的模块装到 `/system_ext/etc/xkernel_vm/modules/`（不设置这个目录就构建报错，见 `product/README.md`）；本仓库带加载顺序 `etc/modules.load`。设备节点的类型 `xhyper_device` 和标签也在本仓库的 system_ext 策略里，因为驱动随本包交付，而不是随厂商分区。若产品改由厂商分区交付驱动（例如 vendor_dlkm），类型、标签和权限就应改由厂商策略和厂商的 ueventd 规则给出。

## 客户机根文件系统

可写的根文件系统 `/data/misc/xkernel_vm/rootfs.img` 在全新设备上并不存在。实例配置给它指定了模板 `/system_ext/etc/xkernel_vm/rootfs.img`（随镜像交付），第一次启动前服务把模板复制过去：全零的块跳过不写，所以复制件是稀疏文件；先写到临时文件，同步后再改名，中途失败不会留下半个镜像。已经存在的盘是客户机的数据，不会再被覆盖。T11206 上 1 GiB 的模板实际写入 51 MiB，用时约 6 秒。

## 只读监视接口

`IXKernelVmMonitor`（服务名 `com.kylinos.xkernelvm.IXKernelVmMonitor/default`）给监视器应用提供两组累计值，速率由客户端用两次读数算出，服务不为客户端保存状态：

- `getEl2Counters()`：每个在线核在 EL2 用掉的周期数和读数时刻，代读宿主驱动的 `/proc/xcore_stats`。
  - 只有启动路径已经允许监视且计数文件存在时，第一次取数才启动计数；监视接口从不请求加载程序做任何事情。`/proc/xcore_stats` 不存在时返回 `EL2_UNAVAILABLE`，原因是“driver not loaded: the VM is off”；不可读取或启动时返回具体原因。模块消失会清空计数运行标志、重试退避和旧样本，下一次加载从新状态开始。
  - 启动计数失败的原因记 5 秒，期间直接回答原因。权限不足时同样返回失败，不请求加载程序修复权限。
  - 只在需要时重新写 1，至多每 5 秒一次：某个在线核的读数连续两次缺失（下线的核不算），读取失败，或者所有核的计数 1 秒没动（被别人写了 0，或驱动被重新加载）。运行中写 1 并不便宜：只要有核在深度空闲里，驱动就会重新下发全局控制，唤醒所有核，还会把 EL2 时间加到正在测的读数上。
  - 15 秒没有人取数就写 0 停止计数，把 PMU 还给宿主（失败时下次再试，最多 5 次）；服务启动时也停一次，收回上一个服务实例可能留下的计数。
  - 周期数按核当时的频率计数，换算成占用要选定一个频率，选哪个由客户端决定。驱动按虚拟化层的核号列出，T11206 上与 Linux 的核号相同，客户端按此使用。
- `getVmCpuTime()`：x-kernel 虚拟机全部 crosvm 进程的 utime + stime 之和。进程包括启动时记录的，加上按进程名 `crosvm_<name>` 扫描到的，扫描结果保留 5 秒：crosvm 在启动后才派生运行虚拟处理器的子进程，直接后端的启动又在启动器执行 crosvm 之前就返回，所以只靠启动时的记录会漏掉它们。读数附带进程集合的标识 `processSet`；集合变化时（例如启动后才扫描到子进程），客户端要像换了实例一样重新取基准。x-kernel 的虚拟处理器由宿主线程代为调度，客户机运行的时间就记在这些线程上，其中也包含虚拟化层替它处理异常的时间和 crosvm 的设备模拟时间。

调用方须是 root、system，或持有 `com.kylinos.xkernelvm.permission.READ_VM_STATS`（或控制权限）。这些是系统级的处理器占用数据，平台策略不让普通应用读 `/proc/stat` 一类数据，所以接口不对所有应用开放。权限由控制应用声明，保护级别 `signature|privileged|development`：平台签名的应用和特权应用（装在 priv-app 下，产品镜像里的监视器就是这样装的）开机即获得，不需要 privapp 白名单（白名单只管平台定义的权限）；调试设备上 adb 装的普通应用用 `pm grant <包名> com.kylinos.xkernelvm.permission.READ_VM_STATS` 授予。SELinux 上监视接口的类型带 `app_api_service` 属性，应用可以查到并调用；`/proc/xcore_stats` 有自己的标签 `proc_xcore_stats`，只有服务能读写、加载程序能改属主。命令行 `xkernel_vmctl monitor [--count N] [--interval-ms MS]` 打印原始读数。

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
| `src/service.rs` | `IXKernelVmControl` 与 `IXKernelVmMonitor` 的 Binder 实现与调用方校验。 |
| `src/monitor.rs` | 监视接口的数据：EL2 周期计数的启停与读取、x-kernel 虚拟机进程的处理器时间。 |
| `guest/` | 客户机里用的工具：负载模拟 `xkload.sh`，以及把它放进根文件系统模板的 `add-to-rootfs.sh`（`debugfs`，不需要 root）；说明见其 `README.md`。 |
| `product/` | 给做镜像的同事：集成说明 `README.md`（源码交付和二进制交付两种方式）；源码交付时预编译目录用的 Android.bp 模板（crosvm）；`binary/`：生成二进制交付目录的 `make-drop.sh` 及其 Android.bp、product.mk 模板；`monitor/`：监视器按 Kylin 应用惯例导入时用的 Android.bp 模板。 |
| `ctl/main.rs` | 命令行客户端 `xkernel_vmctl status|start|stop|watch|console|monitor`。 |
| `etc/` | 实例配置（`xkernel_vm.json` 为直接后端，`xkernel_vm.avf.json` 为经 AVF）、AVF 声明、init 脚本。 |
| `sepolicy/` | system_ext 策略：服务的域、两个接口（监视接口应用可查到）、数据目录、`/proc/xcore_stats` 的标签（`genfs_contexts`）；启动器转入 `crosvm` 域、crosvm 程序本身的类型；加载程序的域、模块文件、驱动属性；设备节点 `xhyper_device`（只有 crosvm 域能打开）。 |
| `src/driver.rs` | 虚拟机持有宿主驱动：串行加载、回收后的卸载、等待结果。 |
| `launcher/` | `xkernel_crosvm_launcher`：进入 `crosvm` 域后重新设置父进程死亡信号，再执行 crosvm。 |
| `loader/` | `xkernel_vm_loader`：按 `modules.load` 加载或逆序卸载模块、准备设备节点、报告结果（逻辑在 `loader.rs`，可在开发机测试）。 |
| `product.mk` | 源码交付的产品集成片段：源码编译的模块、策略目录，以及预编译目录 `XKERNEL_VM_PREBUILTS_DIR` 里的驱动模块、客户机内核与磁盘镜像和 crosvm（见 `product/README.md`）。二进制交付用交付目录里生成的 `product.mk`。 |
| `device/t11206/` | 在 T11206 调试平板上不刷机、按产品方式安装（init 拉起、SELinux 生效）的脚本，以及并入系统扩展分区的策略增量 `system_ext_additions.cil`；说明见其 `README.md`。 |
| `host-tests/` | 在开发机上运行纯逻辑模块的单元测试：`cd host-tests && cargo test`；GitHub 在每次推送和拉取请求时运行它，并检查格式和 clippy（`.github/workflows/host-tests.yml`）。服务本身只能在 Android 源码树里编译，不在 CI 中构建。 |

## 构建

放进与设备版本一致的 Android 源码树后：`m xkernel_vm_service xkernel_vmctl`；应用在独立仓库 `t11206-pvm-app`。

## 已知限制

- 宿主驱动的 `xhyper` 是永久模块，`xhyper_hypercall` 被它引用，关闭开关后这两个模块仍然存在；可卸载的五个模块和两个数据入口必须消失。2026-10-27，Claude 在 T11206HXH6600007 上使用初版提交 `b5b231c` 完成同次开机重载和监视器轮询下 25 次启停；本轮新增的重启残留自动核实路径仍待板测。主机测试只验证调度顺序、错误处理和状态转换。卸载若失败，服务记录原因并让开关落到关闭状态，不能保证内核中所有可卸载模块已经消失；监视接口会保持不可用，下一实例结束时重试。
- 上述板测每次卸载都出现三个 `vm_mgr.c:187 xhyper_vm_function_unregister` 警告，分别来自事件通知和虚拟处理器功能模块。驱动检查模块引用计数时，内核已经减掉基础引用，读数为 -1，触发了现有的警告条件；当时 `panic_on_warn=0`。这是宿主驱动的行为，本次没有改驱动；如果运行环境将警告改为触发 panic，仍有相应风险。

- 经 AVF 时，能力属性目前没有来源：虚拟化层的资源管理器还不计算它，开发验证时由 root 设置一次。直接后端不用它。
- 经 AVF 时，AVF 对受保护虚拟机的磁盘镜像有 SELinux 标签检查；放在 `/data/misc/xkernel_vm` 的可写 rootfs 不在允许列表内，正式布局待定。
- 服务以 `system` 用户运行，没有特权；宿主驱动按可锁定内存上限固定客户机内存，所以 init 脚本把上限设为不限（`rlimit memlock`）。经 AVF 的方式在非 root 下还没有验证过。加载程序以 root 运行，只有加载模块和改设备节点属主的特权。
- SELinux：策略经平台 neverallow 编译检查；2026-10-16 在平板上运行时加载平板策略加本策略，宽容模式下服务和 crosvm 没有一次访问被拒绝（启动器与按需加载另见实施记录）。还没有在强制模式、也没有由 init 启动的服务上验证。
- 就绪：产品配置以串口输出里出现客户机的 shell 提示符 `kylin-x:~#` 作为就绪信号（`readiness.console_marker`），客户机不需要任何改动；提示符变了要同步改配置。也可以用 vsock 端口（`readiness.vsock_port`），但需要客户机监听，且在 SELinux 下只有经 AVF 时能用（平台策略只允许 AVF 的管理进程用 vsock）。没有配置时实例停在“运行中，服务未就绪”。
- 无固件模式不经固件认证镜像，也不传递 DICE 身份；宿主复位时不清理该虚拟机内存（与带固件路径不同）。
- 直接后端：用平台自己的 crosvm（`/apex/com.android.virt/bin/crosvm`，配置里 `hypervisor` 为 `xhyper`，即 crosvm 的 `--hypervisor xhyper`），需要 com.android.virt 由开启了 `xhyper` 功能的 external/crosvm 编出（kylin100-pad-dev，Gerrit 324346–324349）；平台策略只允许 AVF 的 `crosvm` 域打开 `/dev/vhost-vsock` 和虚拟机管理设备，所以 crosvm 由服务转入这个域运行；服务的就绪握手不能用服务自己发起的 vsock 连接（平台策略只允许 AVF 的管理进程、adbd 和 traced 用 vsock）；配置里没有 `launcher` 时，crosvm 直接由服务启动，转入 `crosvm` 域后父进程死亡信号被清除，服务被杀死后只有 init 结束进程组这一层保护；之前用的预编译 crosvm 在 vCPU 出错后也以 33 退出，结束原因会被记成“客户机关机”，平台 crosvm 是否相同待核实。
- 监视接口给出的 x-kernel 处理器时间是宿主上 crosvm 进程的时间：分不出客户机自己运行、虚拟化层替它处理异常和 crosvm 设备模拟各占多少，也分不出它在哪个物理核上运行；要按物理核拆分，只能由虚拟化层按虚拟机分别计时。
- 直接后端的启动在启动器执行 crosvm 之前就返回，所以启动后立即按进程名查找时找不到 crosvm（日志“no crosvm_xkernel_vm process found right after the start”），`instance.json` 里的进程记录为空。回收核实和监视接口另外按进程名扫描，不受影响；服务重启后核实上一轮进程也靠扫描。
