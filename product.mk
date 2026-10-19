# Include from the product makefile of a device whose host runs the XHyper driver. Everything
# goes to the system_ext partition; the vendor partition is not touched. product/README.md is the
# integration guide for image builders.
#
# The instance config runs the VM with the direct backend: the service starts the crosvm build
# with the hypervisor's backend itself (/system_ext/bin/xkernel_crosvm, a prebuilt). Running it
# through AVF instead needs the Virtualization module's support for declared firmware-less VMs,
# etc/xkernel_vm.avf.json as the config and protected_vms_without_firmware_xkernel.xml in
# PRODUCT_PACKAGES.

# Built from source: the service (it pulls in its launcher, driver loader, modules.load and init
# script), its instance config, and the control app (repository t11206-pvm-app, which also
# declares the permissions CONTROL_VM and READ_VM_STATS).
PRODUCT_PACKAGES += \
    xkernel_vm_service \
    xkernel_vm.json \
    XKernelVmControl

# The shell client, for debugging builds only.
PRODUCT_PACKAGES_DEBUG += xkernel_vmctl

SYSTEM_EXT_PRIVATE_SEPOLICY_DIRS += $(dir $(lastword $(MAKEFILE_LIST)))sepolicy/private

# Everything that is not built from source comes from one directory the image build names in
# XKERNEL_VM_PREBUILTS_DIR (it must be inside the source tree, as it holds an Android.bp):
#
#   modules/<each .ko of etc/modules.load>  the hypervisor host driver, built with the device's
#                                           kernel; xkernel_vm_loader loads them on first use
#   kernel.bin, dummy.img                   the guest kernel and its read-only disk
#   rootfs.img                              the guest's root file system: the template the service
#                                           copies to /data/misc/xkernel_vm on the first start
#   xkernel_crosvm                          crosvm with the hypervisor's backend (statically linked)
#   Android.bp                              this repository's product/prebuilts.bp.template
#
# The Android.bp there turns xkernel_crosvm into a module: an ELF under bin cannot go through
# PRODUCT_COPY_FILES. The monitor app (OS之OS监控软件, com.kylinos.monitor) is not part of this
# package: it is imported like the product's other Kylin apps (product/monitor/).
ifeq ($(XKERNEL_VM_PREBUILTS_DIR),)
$(error XKERNEL_VM_PREBUILTS_DIR must name the directory of the x-kernel VM prebuilts (see product/README.md))
endif

XKERNEL_VM_MODULES := \
    xhyper_hypercall.ko \
    xhyper.ko \
    xhyper_platform_hooks.ko \
    xhyper_rsc_mgr.ko \
    xhyper_vcpu.ko \
    xhyper_irqfd.ko \
    xhyper_ioeventfd.ko

PRODUCT_PACKAGES += xkernel_crosvm

# Plain files under etc (the ELF check of PRODUCT_COPY_FILES covers bin and lib paths only).
PRODUCT_COPY_FILES += \
    $(foreach m,$(XKERNEL_VM_MODULES),$(XKERNEL_VM_PREBUILTS_DIR)/modules/$(m):$(TARGET_COPY_OUT_SYSTEM_EXT)/etc/xkernel_vm/modules/$(m)) \
    $(foreach f,kernel.bin dummy.img rootfs.img,$(XKERNEL_VM_PREBUILTS_DIR)/$(f):$(TARGET_COPY_OUT_SYSTEM_EXT)/etc/xkernel_vm/$(f))
