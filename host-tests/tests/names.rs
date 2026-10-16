//! The service, the loader, the init script and the property contexts name the same properties,
//! and the product makefile installs the modules modules.load lists.
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

#[test]
fn the_product_installs_the_listed_modules() {
    let listed: Vec<String> = repo_file("etc/modules.load")
        .lines()
        .filter_map(|l| l.split('#').next().unwrap().split_whitespace().next().map(str::to_string))
        .collect();
    let mk = repo_file("product.mk");
    let start = mk.find("XKERNEL_VM_MODULES :=").expect("module list in product.mk");
    let installed: Vec<String> = mk[start..]
        .lines()
        .skip(1)
        .map(|l| l.trim().trim_end_matches('\\').trim().to_string())
        .take_while(|l| !l.is_empty())
        .collect();
    assert_eq!(installed, listed);
}
