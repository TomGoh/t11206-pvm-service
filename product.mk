# Include from the product makefile of a device whose host runs the XHyper driver.
#
# The instance config runs the VM with the direct backend: the service starts the crosvm build
# with the hypervisor's backend itself, which the product installs as /system_ext/bin/xkernel_crosvm
# (not part of this repository). Running it through AVF instead needs the Virtualization module's
# support for declared firmware-less VMs, etc/xkernel_vm.avf.json as the config and
# protected_vms_without_firmware_xkernel.xml in PRODUCT_PACKAGES.
PRODUCT_PACKAGES += \
    xkernel_vm_service \
    xkernel_vmctl \
    xkernel_vm.json

SYSTEM_EXT_PRIVATE_SEPOLICY_DIRS += $(dir $(lastword $(MAKEFILE_LIST)))sepolicy/private
