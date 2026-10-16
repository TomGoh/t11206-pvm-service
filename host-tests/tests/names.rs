//! The service, the loader, the init script and the property contexts name the same properties.
use std::fs;
use xkernel_vm_service_host_tests::{driver, loader};

fn repo_file(path: &str) -> String {
    fs::read_to_string(format!("{}/../{path}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

#[test]
fn the_driver_properties_agree() {
    assert_eq!(driver::LOAD_PROPERTY, loader::LOAD_PROPERTY);
    assert_eq!(driver::STATUS_PROPERTY, loader::STATUS_PROPERTY);
    let rc = repo_file("etc/init/xkernel_vm_service.rc");
    assert!(
        rc.contains(&format!("on property:{}=*", driver::LOAD_PROPERTY)),
        "init script trigger"
    );
    let contexts = repo_file("sepolicy/private/property_contexts");
    for name in [driver::LOAD_PROPERTY, driver::STATUS_PROPERTY] {
        assert!(
            contexts.lines().any(|l| l.split_whitespace().next() == Some(name)),
            "{name} in property_contexts"
        );
    }
}
