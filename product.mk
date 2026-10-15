# Include from the product makefile of a device whose host runs the XHyper driver.
PRODUCT_PACKAGES += \
    xkernel_vm_service \
    xkernel_vmctl \
    xkernel_vm.json \
    protected_vms_without_firmware_xkernel.xml

SYSTEM_EXT_PRIVATE_SEPOLICY_DIRS += $(dir $(lastword $(MAKEFILE_LIST)))sepolicy/private
