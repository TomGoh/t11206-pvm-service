# Vendor policy for the hypervisor device node

The XHyper host driver is a vendor module, so its device node is labelled by vendor policy, not
by this system_ext policy. Platform policy (system/sepolicy, private/crosvm.te) lets the `crosvm`
domain open every type with the public attribute `vm_manager_device_type` and forbids every
other system-side domain to open one. `kvm_device` itself is a private platform type that vendor
policy cannot name, so the vendor defines its own type with the attribute:

```
# vendor sepolicy
type xhyper_device, dev_type, vm_manager_device_type;

# vendor file_contexts
/dev/xhyper  u:object_r:xhyper_device:s0
```

and, so that the service need not run as root, a ueventd rule in the vendor `ueventd.rc`:

```
/dev/xhyper  0660  root  virtualmachine
```

Neither is applied yet: on the tablet the node is `crw------- root root u:object_r:device:s0`
(2026-10-15), a label platform policy forbids every domain to open (private/domain.te), and
SELinux is permissive, which is why the service runs as root.

The direct backend starts crosvm itself. Only virtmgr may execute AVF's `crosvm_exec`, and only
the `crosvm` domain may open `kvm_device` (`/dev/vhost-vsock`), so the product's crosvm prebuilt
needs its own exec type and a transition from this service into the `crosvm` domain, with the
rules for the service's pipes, data directory and control socket. That policy is not written yet.
