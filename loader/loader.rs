// Copyright 2026 KylinSoft Co., Ltd.
// SPDX-License-Identifier: Apache-2.0

//! What `xkernel_vm_loader` does, apart from the system calls (see `main.rs`), so it is tested on
//! a development host.
//!
//! The module directory holds the driver's `.ko` files and `modules.load`, which lists them in
//! load order, one per line, optionally followed by module parameters; `#` starts a comment. A
//! module already loaded (`/sys/module/<name>`; the hypervisor's core modules cannot be unloaded)
//! is skipped, so a load that failed half-way can be retried. The first failure stops the load.
//! After the last module the loader waits for the device node, which ueventd creates, and gives
//! it its owner and mode.

use std::io;
use std::path::Path;
use std::time::Duration;

/// Written by the control service to ask for a load (also in its `driver.rs` and the init script).
pub const LOAD_PROPERTY: &str = "xkernel_vm.load_driver";
/// Written by the loader: `loaded <request>` or `failed <request>: <reason>`.
pub const STATUS_PROPERTY: &str = "xkernel_vm.driver_status";

/// One line of `modules.load`.
#[derive(Debug, PartialEq, Eq)]
pub struct Module {
    pub file: String,
    pub params: String,
}

impl Module {
    /// The kernel's name for the module: the file name without `.ko`, with `-` as `_`.
    pub fn name(&self) -> String {
        self.file.trim_end_matches(".ko").replace('-', "_")
    }
}

pub fn parse_list(text: &str) -> Result<Vec<Module>, String> {
    let mut modules = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let (file, params) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        if !file.ends_with(".ko") || file.contains('/') {
            return Err(format!(
                "modules.load line {}: {file:?} is not a .ko in this directory",
                n + 1
            ));
        }
        modules.push(Module { file: file.to_string(), params: params.trim().to_string() });
    }
    if modules.is_empty() {
        return Err("modules.load lists no module".into());
    }
    Ok(modules)
}

/// The system calls the load needs.
pub trait Kernel {
    fn is_loaded(&self, name: &str) -> bool;
    /// Loads the module file with its parameters (`finit_module`).
    fn load(&self, path: &Path, params: &str) -> io::Result<()>;
    fn node_exists(&self) -> bool;
    /// Gives the device node its owner and mode.
    fn prepare_node(&self) -> io::Result<()>;
    fn sleep(&self, duration: Duration);
}

const NODE_POLL: Duration = Duration::from_millis(50);

/// Loads `modules` from `dir` in order and prepares the device node. Returns the modules it
/// loaded (the others were there already).
pub fn load_all(
    dir: &Path,
    modules: &[Module],
    kernel: &dyn Kernel,
    node_timeout: Duration,
) -> Result<Vec<String>, String> {
    let mut loaded = Vec::new();
    for module in modules {
        let name = module.name();
        if kernel.is_loaded(&name) {
            continue;
        }
        match kernel.load(&dir.join(&module.file), &module.params) {
            Ok(()) => loaded.push(name),
            // Loaded by someone else meanwhile.
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
            Err(e) => return Err(format!("{}: {e}", module.file)),
        }
    }
    let mut waited = Duration::ZERO;
    while !kernel.node_exists() {
        if waited >= node_timeout {
            return Err(format!("no device node {}s after loading", node_timeout.as_secs()));
        }
        kernel.sleep(NODE_POLL);
        waited += NODE_POLL;
    }
    kernel.prepare_node().map_err(|e| format!("device node: {e}"))?;
    Ok(loaded)
}

/// `text` cut to at most `max` bytes on a character boundary (system property values are short).
pub fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::path::PathBuf;

    #[derive(Default)]
    struct FakeKernel {
        present: RefCell<Vec<String>>,
        calls: RefCell<Vec<(PathBuf, String)>>,
        fail: Option<(&'static str, i32)>,
        node_after_polls: Cell<i32>,
        prepared: Cell<bool>,
    }

    impl Kernel for FakeKernel {
        fn is_loaded(&self, name: &str) -> bool {
            self.present.borrow().iter().any(|p| p == name)
        }
        fn load(&self, path: &Path, params: &str) -> io::Result<()> {
            self.calls.borrow_mut().push((path.to_path_buf(), params.to_string()));
            let file = path.file_name().unwrap().to_str().unwrap();
            if let Some((bad, errno)) = self.fail {
                if file == bad {
                    return Err(io::Error::from_raw_os_error(errno));
                }
            }
            self.present.borrow_mut().push(file.trim_end_matches(".ko").to_string());
            Ok(())
        }
        fn node_exists(&self) -> bool {
            let left = self.node_after_polls.get();
            self.node_after_polls.set(left - 1);
            left <= 0
        }
        fn prepare_node(&self) -> io::Result<()> {
            self.prepared.set(true);
            Ok(())
        }
        fn sleep(&self, _: Duration) {}
    }

    const LIST: &str = "# the hypervisor host driver, in dependency order\n\
        xhyper_hypercall.ko\nxhyper.ko\n\nxhyper_platform_hooks.ko\n\
        xhyper_rsc_mgr.ko strip=1 # parameters pass through\nxhyper_vcpu.ko\n";

    #[test]
    fn parses_the_list_in_order_with_parameters() {
        let modules = parse_list(LIST).unwrap();
        let files: Vec<_> = modules.iter().map(|m| m.file.as_str()).collect();
        assert_eq!(
            files,
            [
                "xhyper_hypercall.ko",
                "xhyper.ko",
                "xhyper_platform_hooks.ko",
                "xhyper_rsc_mgr.ko",
                "xhyper_vcpu.ko"
            ]
        );
        assert_eq!(modules[3].params, "strip=1");
        assert_eq!(Module { file: "a-b.ko".into(), params: String::new() }.name(), "a_b");
        for bad in ["", "# nothing\n", "xhyper\n", "../x.ko\n", "/abs/x.ko\n"] {
            assert!(parse_list(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn loads_in_order_skips_loaded_and_prepares_the_node() {
        let kernel = FakeKernel { node_after_polls: Cell::new(3), ..Default::default() };
        kernel.present.borrow_mut().push("xhyper_hypercall".into());
        let loaded =
            load_all(Path::new("/m"), &parse_list(LIST).unwrap(), &kernel, Duration::from_secs(1))
                .unwrap();
        assert_eq!(loaded, ["xhyper", "xhyper_platform_hooks", "xhyper_rsc_mgr", "xhyper_vcpu"]);
        let calls = kernel.calls.borrow();
        assert_eq!(calls[0].0, Path::new("/m/xhyper.ko"));
        assert_eq!(calls[2], (PathBuf::from("/m/xhyper_rsc_mgr.ko"), "strip=1".to_string()));
        assert!(kernel.prepared.get());
    }

    #[test]
    fn the_first_failure_stops_the_load() {
        let kernel = FakeKernel {
            fail: Some(("xhyper_platform_hooks.ko", libc::EINVAL)),
            ..Default::default()
        };
        let err =
            load_all(Path::new("/m"), &parse_list(LIST).unwrap(), &kernel, Duration::from_secs(1))
                .unwrap_err();
        assert!(err.starts_with("xhyper_platform_hooks.ko: "), "{err}");
        assert_eq!(kernel.calls.borrow().len(), 3, "nothing after the failed module");
        assert!(!kernel.prepared.get());
    }

    #[test]
    fn a_module_loaded_meanwhile_is_not_a_failure() {
        let kernel = FakeKernel { fail: Some(("xhyper.ko", libc::EEXIST)), ..Default::default() };
        assert!(load_all(
            Path::new("/m"),
            &parse_list(LIST).unwrap(),
            &kernel,
            Duration::from_secs(1)
        )
        .is_ok());
    }

    #[test]
    fn a_node_that_never_comes_is_reported() {
        let kernel = FakeKernel { node_after_polls: Cell::new(i32::MAX), ..Default::default() };
        let err =
            load_all(Path::new("/m"), &parse_list(LIST).unwrap(), &kernel, Duration::from_secs(1))
                .unwrap_err();
        assert!(err.contains("no device node"), "{err}");
        assert!(!kernel.prepared.get());
    }

    #[test]
    fn clips_on_a_character_boundary() {
        assert_eq!(clip("abc", 5), "abc");
        assert_eq!(clip("ab模块", 4), "ab");
        assert_eq!(clip("ab模块", 5), "ab模");
    }
}
