//! The service, the loader, the init script and the property contexts name the same properties;
//! the product makefile installs the modules modules.load lists, and the files the instance config
//! names are where the product makefile and the prebuilts template install them; the binary
//! delivery declares the modules of the source build, and the monitor imports like a Kylin app.
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

#[test]
fn the_product_installs_what_the_config_names() {
    let config: serde_json::Value =
        serde_json::from_str(&repo_file("etc/xkernel_vm.json")).unwrap();
    let mk = repo_file("product.mk");
    let etc = "/system_ext/etc/xkernel_vm/";
    // product.mk copies these names from the prebuilts directory to /system_ext/etc/xkernel_vm.
    assert!(
        mk.contains("$(foreach f,kernel.bin dummy.img rootfs.img,"),
        "guest images in product.mk"
    );
    assert_eq!(config["kernel"], format!("{etc}kernel.bin"));
    let disks = config["disks"].as_array().unwrap();
    assert_eq!(disks[0]["path"], format!("{etc}dummy.img"));
    assert_eq!(disks[1]["template"], format!("{etc}rootfs.img"));
    // crosvm is the platform's (com.android.virt, built with its xhyper feature), told to use
    // XHyper; the product ships no crosvm of its own, and the policy lets the crosvm domain, which
    // the launcher enters, execute crosvm_exec.
    assert_eq!(config["direct"]["crosvm"], "/apex/com.android.virt/bin/crosvm");
    assert_eq!(config["direct"]["hypervisor"], "xhyper");
    assert!(
        !mk.contains("xkernel_crosvm\n") && !mk.contains("xkernel_crosvm \\"),
        "no crosvm prebuilt"
    );
    assert!(!mk.contains("Monitor"), "no monitor app here");
    let contexts = repo_file("sepolicy/private/file_contexts");
    assert!(!contexts.contains("/system_ext/bin/xkernel_crosvm "), "no crosvm prebuilt label");
    let te = repo_file("sepolicy/private/xkernel_vm_service.te");
    assert!(te.contains("allow crosvm crosvm_exec:file { rx_file_perms execute_no_trans };"));
}

/// The modules of an Android.bp: (type, name, body), for blocks that start in column 0.
fn bp_modules(bp: &str) -> Vec<(String, String, String)> {
    let mut modules = Vec::new();
    let mut current: Option<(String, String)> = None;
    for line in bp.lines() {
        if let Some((kind, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
            if line == "}" {
                let name = body
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("name: \""))
                    .map(|n| n.trim_end_matches("\",").to_string())
                    .unwrap_or_default();
                modules.push((kind.clone(), name, body.clone()));
                current = None;
            }
        } else if let Some(kind) = line.strip_suffix(" {") {
            if !kind.starts_with(' ') && !kind.starts_with('/') && kind != "package" {
                current = Some((kind.to_string(), String::new()));
            }
        }
    }
    modules
}

/// The quoted strings of a list property (`required: [...]`) in a module body.
fn bp_list(body: &str, property: &str) -> Vec<String> {
    let start = match body.find(&format!("{property}: [")) {
        Some(start) => start,
        None => return Vec::new(),
    };
    let list = &body[start..start + body[start..].find(']').unwrap()];
    list.split('"').skip(1).step_by(2).map(str::to_string).collect()
}

/// The modules a product makefile puts in `variable`, in either the one-line or the
/// backslash-continued form.
fn mk_packages(mk: &str, variable: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut lines = mk.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix(&format!("{variable} +=")) {
            let mut text = rest.to_string();
            while text.trim_end().ends_with('\\') {
                text = text.trim_end().trim_end_matches('\\').to_string();
                text.push(' ');
                text.push_str(lines.next().unwrap_or(""));
            }
            names.extend(text.split_whitespace().map(str::to_string));
        }
    }
    names.sort();
    names
}

#[test]
fn the_binary_delivery_matches_the_source_build() {
    let source = bp_modules(&repo_file("Android.bp"));
    let binary = bp_modules(&repo_file("product/binary/Android.bp.template"));
    let source_mk = repo_file("product.mk");
    // What the source build installs: product.mk's packages and, transitively, what this
    // repository's modules among them require. The control app (t11206-pvm-app) is listed there
    // too.
    let mut want: Vec<String> = mk_packages(&source_mk, "PRODUCT_PACKAGES");
    want.extend(mk_packages(&source_mk, "PRODUCT_PACKAGES_DEBUG"));
    let mut next = 0;
    while next < want.len() {
        if let Some((_, _, body)) = source.iter().find(|(_, n, _)| *n == want[next]) {
            for required in bp_list(body, "required") {
                if !want.contains(&required) {
                    want.push(required);
                }
            }
        }
        next += 1;
    }
    want.sort();
    let installed: Vec<&(String, String, String)> =
        source.iter().filter(|(_, name, _)| want.contains(name)).collect();
    let mut have: Vec<String> = binary.iter().map(|(_, name, _)| name.clone()).collect();
    have.sort();
    assert_eq!(have, want, "module names of the binary delivery");
    for (kind, name, body) in &installed {
        let (_, _, prebuilt) = binary.iter().find(|(_, n, _)| n == name).unwrap();
        // Same partition, install path, init script and pulled-in modules.
        for property in ["system_ext_specific: true", "relative_install_path", "filename"] {
            let line = |b: &str| {
                b.lines().find(|l| l.contains(property)).map(str::trim).map(str::to_string)
            };
            assert_eq!(line(prebuilt), line(body), "{name}: {property}");
        }
        assert_eq!(bp_list(prebuilt, "required"), bp_list(body, "required"), "{name}: required");
        if kind == "rust_binary" {
            assert_eq!(
                bp_list(prebuilt, "init_rc").len(),
                bp_list(body, "init_rc").len(),
                "{name}: init_rc"
            );
        }
    }
    let (_, _, app) = binary.iter().find(|(_, n, _)| n == "XKernelVmControl").unwrap();
    assert!(app.contains("certificate: \"platform\"") && app.contains("system_ext_specific: true"));
    // The same product configuration.
    let binary_mk = repo_file("product/binary/product.mk.template");
    for variable in ["PRODUCT_PACKAGES", "PRODUCT_PACKAGES_DEBUG"] {
        assert_eq!(
            mk_packages(&binary_mk, variable),
            mk_packages(&source_mk, variable),
            "{variable}"
        );
    }
    assert!(binary_mk.contains("SYSTEM_EXT_PRIVATE_SEPOLICY_DIRS +="), "policy sources");
    assert!(binary_mk.contains("$(foreach f,kernel.bin dummy.img rootfs.img,"), "guest images");
}

#[test]
fn the_monitor_imports_like_the_kylin_apps() {
    let modules = bp_modules(&repo_file("product/monitor/Android.bp.template"));
    assert_eq!(modules.len(), 1);
    let (kind, name, body) = &modules[0];
    assert_eq!((kind.as_str(), name.as_str()), ("android_app_import", "KylinosMonitor"));
    for line in [
        "apk: \"KylinosMonitor.apk\"",
        "certificate: \"platform\"",
        "privileged: true",
        "product_specific: true",
    ] {
        assert!(body.contains(line), "{line}");
    }
}
