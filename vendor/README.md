# Vendor side: the hypervisor device node

The XHyper host driver is a vendor module, so its device node `/dev/xhyper` is labelled and
permissioned by the vendor partition, not by this repository's system_ext policy. Without this the
node keeps the generic `device` label, which platform policy forbids every domain to open
(system/sepolicy private/domain.te), and root-only permissions: under enforcing SELinux nobody
could run a VM, through AVF or not.

| File | Goes into | Content |
|---|---|---|
| `sepolicy/xhyper_device.te`, `sepolicy/file_contexts` | `BOARD_VENDOR_SEPOLICY_DIRS` of the product | `type xhyper_device, dev_type, vm_manager_device_type;` and the label of `/dev/xhyper`. |
| `ueventd.rc` | the vendor `ueventd.rc` (`/vendor/etc/ueventd.rc`; ueventd reads only that one from the vendor partition, so the line is merged into it) | `/dev/xhyper 0666 root root`. |

Why these values:

- **The type.** Platform policy lets the `crosvm` domain open every type with the public attribute
  `vm_manager_device_type` and forbids every other core or app domain to open one, or even to
  `stat` it (private/crosvm.te). AVF's crosvm and the control service's direct-backend crosvm
  both run in `crosvm`, so both may use the node and nothing else on the system side may.
  `kvm_device` itself is a private platform type that vendor policy cannot name.
- **The mode.** As AOSP's `/dev/kvm` and `/dev/vhost-vsock` (system/core/rootdir/ueventd.rc):
  crosvm runs as whoever starts it, an app through AVF or the control service as `system`, so the
  file mode stays open and SELinux does the limiting.

Checked on the T11206 tablet on 2026-10-16 (record `t11206-avf-nofw-impl-20261015`, board/35): with
the tablet's policy plus this type and the service's policy loaded at runtime, the node labelled
`xhyper_device` and set to 0666 root root, the service running as `system` in its own domain
started, stopped and restarted the VM with crosvm in the `crosvm` domain, and no access of the
service or of crosvm was refused (permissive, so every missing rule would have been logged). These
files themselves have not been built in a vendor tree yet; the trial loaded their CIL equivalent.
