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

# The hypervisor host driver's modules, in the order of etc/modules.load. They are built with the
# device's kernel and are not kept in this repository: the image build sets
# XKERNEL_VM_MODULES_DIR to the directory holding them, and they are installed beside
# modules.load in /system_ext/etc/xkernel_vm/modules, where xkernel_vm_loader loads them on the
# first start after boot. (The ELF check of PRODUCT_COPY_FILES covers bin and lib paths only.)
XKERNEL_VM_MODULES := \
    xhyper_hypercall.ko \
    xhyper.ko \
    xhyper_platform_hooks.ko \
    xhyper_rsc_mgr.ko \
    xhyper_vcpu.ko \
    xhyper_irqfd.ko \
    xhyper_ioeventfd.ko

ifeq ($(XKERNEL_VM_MODULES_DIR),)
$(error XKERNEL_VM_MODULES_DIR must name the directory of the hypervisor driver modules ($(XKERNEL_VM_MODULES)))
endif
PRODUCT_COPY_FILES += \
    $(foreach m,$(XKERNEL_VM_MODULES),$(XKERNEL_VM_MODULES_DIR)/$(m):$(TARGET_COPY_OUT_SYSTEM_EXT)/etc/xkernel_vm/modules/$(m))
