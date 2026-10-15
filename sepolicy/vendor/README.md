# Vendor policy for the hypervisor device node

The XHyper host driver is a vendor module, so its device node is labelled by vendor policy, not
by this system_ext policy. crosvm (domain `crosvm`) already may open `kvm_device`; labelling the
node with it lets AVF's crosvm use it without new rules:

```
# vendor file_contexts
/dev/xhyper  u:object_r:kvm_device:s0
```

and, so that the service need not run as root, a ueventd rule in the vendor `ueventd.rc`:

```
/dev/xhyper  0660  root  virtualmachine
```

Neither is applied yet: on the tablet the node is `crw------- root root u:object_r:device:s0`
(2026-10-15) and SELinux is permissive, which is why the service runs as root.
