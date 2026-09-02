//! CLI `--install` / `--uninstall` drivers for the installer.
//!
//! Same split as `apply.rs`: the exit-code and ordering logic is a
//! driver over the [`CliOps`] trait so it can be unit-tested without
//! touching the real registry. [`RealCliOps`] is the thin production
//! implementation that forwards to `arcthumb::registry`.
//!
//! Exit codes (consumed by the Inno Setup installer — keep stable):
//! - `0` success
//! - `2` arcthumb.dll not found (`--install` only)
//! - `3` CLSID registration failed
//! - `4` extension binding failed
//!
//! (Exit code `5` — GUI init failure — lives in `main.rs`; it is not
//! part of the CLI drivers.)

use std::io;
use std::path::{Path, PathBuf};

use arcthumb::registry::{self, Scope};

use crate::dll_path;

pub const EXIT_OK: i32 = 0;
pub const EXIT_DLL_NOT_FOUND: i32 = 2;
pub const EXIT_CLSID_FAILED: i32 = 3;
pub const EXIT_EXTENSION_FAILED: i32 = 4;

/// The side effects the CLI drivers need. Production uses
/// [`RealCliOps`]; tests inject a recording mock.
pub trait CliOps {
    fn resolve_dll_path(&self) -> Result<PathBuf, String>;
    fn current_scope(&self) -> Scope;
    fn register_clsid(&self, scope: Scope, dll_path: &Path) -> io::Result<()>;
    fn register_preview_clsid(&self, scope: Scope, dll_path: &Path) -> io::Result<()>;
    fn register_preview_handler_list_entry(&self, scope: Scope) -> io::Result<()>;
    fn register_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()>;
    fn register_preview_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()>;
    fn unregister_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()>;
    fn unregister_preview_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()>;
    fn unregister_clsid(&self, scope: Scope) -> io::Result<()>;
    fn unregister_preview_clsid(&self, scope: Scope) -> io::Result<()>;
    fn unregister_preview_handler_list_entry(&self, scope: Scope) -> io::Result<()>;
    fn notify_assoc_changed(&self);
}

pub struct RealCliOps;

impl CliOps for RealCliOps {
    fn resolve_dll_path(&self) -> Result<PathBuf, String> {
        dll_path::resolve_dll_path()
    }
    fn current_scope(&self) -> Scope {
        arcthumb::elevation::current_scope()
    }
    fn register_clsid(&self, scope: Scope, dll_path: &Path) -> io::Result<()> {
        registry::register_clsid(scope, dll_path)
    }
    fn register_preview_clsid(&self, scope: Scope, dll_path: &Path) -> io::Result<()> {
        registry::register_preview_clsid(scope, dll_path)
    }
    fn register_preview_handler_list_entry(&self, scope: Scope) -> io::Result<()> {
        registry::register_preview_handler_list_entry(scope)
    }
    fn register_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
        registry::register_extension(scope, ext)
    }
    fn register_preview_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
        registry::register_preview_extension(scope, ext)
    }
    fn unregister_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
        registry::unregister_extension(scope, ext)
    }
    fn unregister_preview_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
        registry::unregister_preview_extension(scope, ext)
    }
    fn unregister_clsid(&self, scope: Scope) -> io::Result<()> {
        registry::unregister_clsid(scope)
    }
    fn unregister_preview_clsid(&self, scope: Scope) -> io::Result<()> {
        registry::unregister_preview_clsid(scope)
    }
    fn unregister_preview_handler_list_entry(&self, scope: Scope) -> io::Result<()> {
        registry::unregister_preview_handler_list_entry(scope)
    }
    fn notify_assoc_changed(&self) {
        registry::notify_assoc_changed();
    }
}

/// `--install`: write the full shell-extension registration.
///
/// Hive is picked by elevation: HKLM when the process is elevated
/// (admin Inno install mode), HKCU otherwise. This is what makes the
/// shell extension load under High-Integrity Explorer in Windows
/// Sandbox and enterprise lockdowns where HKCU CLSIDs are ignored.
pub fn run_install(ops: &dyn CliOps) -> i32 {
    let dll_path = match ops.resolve_dll_path() {
        Ok(p) => p,
        Err(_) => return EXIT_DLL_NOT_FOUND,
    };
    let scope = ops.current_scope();
    // Both COM classes (thumbnail provider + preview handler) are
    // registered together by the installer so the user gets both
    // features by default. The GUI's "Enable preview pane" checkbox
    // can later be unchecked to remove just the preview handler.
    if ops.register_clsid(scope, &dll_path).is_err() {
        return EXIT_CLSID_FAILED;
    }
    if ops.register_preview_clsid(scope, &dll_path).is_err() {
        return EXIT_CLSID_FAILED;
    }
    if ops.register_preview_handler_list_entry(scope).is_err() {
        return EXIT_CLSID_FAILED;
    }
    for &ext in registry::EXTENSIONS {
        if ops.register_extension(scope, ext).is_err() {
            return EXIT_EXTENSION_FAILED;
        }
        if ops.register_preview_extension(scope, ext).is_err() {
            return EXIT_EXTENSION_FAILED;
        }
    }
    // Tell Explorer to drop its icon/thumbnail cache so the freshly
    // registered handlers take effect without a reboot — this is what
    // Microsoft's shell extension docs require us to do.
    ops.notify_assoc_changed();
    EXIT_OK
}

/// `--uninstall`: clean BOTH hives best-effort. The user may have
/// switched modes between versions, or an old per-user install may
/// still be lying around when a new per-machine install is being
/// uninstalled.
pub fn run_uninstall(ops: &dyn CliOps) -> i32 {
    for scope in Scope::ALL.iter().copied() {
        for &ext in registry::EXTENSIONS {
            let _ = ops.unregister_extension(scope, ext);
            let _ = ops.unregister_preview_extension(scope, ext);
        }
        let _ = ops.unregister_preview_handler_list_entry(scope);
        let _ = ops.unregister_clsid(scope);
        let _ = ops.unregister_preview_clsid(scope);
    }
    ops.notify_assoc_changed();
    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Recording mock. Each side effect is logged as
    /// `"<op>:<scope>[:<arg>]"`; names listed in `fail_on` return an
    /// error after being recorded.
    struct MockCliOps {
        dll_path: Option<PathBuf>,
        scope: Scope,
        calls: RefCell<Vec<String>>,
        fail_on: RefCell<Vec<String>>,
        notify_called: RefCell<bool>,
    }

    impl MockCliOps {
        fn new() -> Self {
            Self {
                dll_path: Some(PathBuf::from(r"C:\fake\arcthumb.dll")),
                scope: Scope::PerUser,
                calls: RefCell::new(Vec::new()),
                fail_on: RefCell::new(Vec::new()),
                notify_called: RefCell::new(false),
            }
        }

        fn without_dll(mut self) -> Self {
            self.dll_path = None;
            self
        }

        fn with_scope(mut self, scope: Scope) -> Self {
            self.scope = scope;
            self
        }

        fn fail_on(self, call: &str) -> Self {
            self.fail_on.borrow_mut().push(call.to_string());
            self
        }

        fn record(&self, name: String) -> io::Result<()> {
            let fail = self.fail_on.borrow().contains(&name);
            self.calls.borrow_mut().push(name);
            if fail {
                Err(io::Error::other("mock failure"))
            } else {
                Ok(())
            }
        }
    }

    fn tag(scope: Scope) -> &'static str {
        match scope {
            Scope::PerUser => "user",
            Scope::PerMachine => "machine",
        }
    }

    impl CliOps for MockCliOps {
        fn resolve_dll_path(&self) -> Result<PathBuf, String> {
            self.dll_path.clone().ok_or_else(|| "not found".to_string())
        }
        fn current_scope(&self) -> Scope {
            self.scope
        }
        fn register_clsid(&self, scope: Scope, _dll_path: &Path) -> io::Result<()> {
            self.record(format!("register_clsid:{}", tag(scope)))
        }
        fn register_preview_clsid(&self, scope: Scope, _dll_path: &Path) -> io::Result<()> {
            self.record(format!("register_preview_clsid:{}", tag(scope)))
        }
        fn register_preview_handler_list_entry(&self, scope: Scope) -> io::Result<()> {
            self.record(format!(
                "register_preview_handler_list_entry:{}",
                tag(scope)
            ))
        }
        fn register_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
            self.record(format!("register_extension:{}:{ext}", tag(scope)))
        }
        fn register_preview_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
            self.record(format!("register_preview_extension:{}:{ext}", tag(scope)))
        }
        fn unregister_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
            self.record(format!("unregister_extension:{}:{ext}", tag(scope)))
        }
        fn unregister_preview_extension(&self, scope: Scope, ext: &'static str) -> io::Result<()> {
            self.record(format!("unregister_preview_extension:{}:{ext}", tag(scope)))
        }
        fn unregister_clsid(&self, scope: Scope) -> io::Result<()> {
            self.record(format!("unregister_clsid:{}", tag(scope)))
        }
        fn unregister_preview_clsid(&self, scope: Scope) -> io::Result<()> {
            self.record(format!("unregister_preview_clsid:{}", tag(scope)))
        }
        fn unregister_preview_handler_list_entry(&self, scope: Scope) -> io::Result<()> {
            self.record(format!(
                "unregister_preview_handler_list_entry:{}",
                tag(scope)
            ))
        }
        fn notify_assoc_changed(&self) {
            *self.notify_called.borrow_mut() = true;
        }
    }

    // ----- run_install ----------------------------------------------------

    #[test]
    fn install_registers_clsids_then_every_extension_and_notifies() {
        let ops = MockCliOps::new();
        assert_eq!(run_install(&ops), EXIT_OK);
        assert!(*ops.notify_called.borrow());

        let calls = ops.calls.borrow();
        // Both CLSIDs and the global preview-handler list entry first.
        assert_eq!(calls[0], "register_clsid:user");
        assert_eq!(calls[1], "register_preview_clsid:user");
        assert_eq!(calls[2], "register_preview_handler_list_entry:user");
        // Then thumbnail + preview bindings for every extension.
        assert_eq!(calls.len(), 3 + registry::EXTENSIONS.len() * 2);
        for (i, &ext) in registry::EXTENSIONS.iter().enumerate() {
            assert_eq!(calls[3 + i * 2], format!("register_extension:user:{ext}"));
            assert_eq!(
                calls[4 + i * 2],
                format!("register_preview_extension:user:{ext}")
            );
        }
    }

    #[test]
    fn install_targets_the_scope_reported_by_elevation() {
        let ops = MockCliOps::new().with_scope(Scope::PerMachine);
        assert_eq!(run_install(&ops), EXIT_OK);
        assert!(
            ops.calls.borrow().iter().all(|c| c.contains(":machine")),
            "every registration must hit the elevated hive"
        );
    }

    #[test]
    fn install_returns_2_when_dll_is_missing() {
        let ops = MockCliOps::new().without_dll();
        assert_eq!(run_install(&ops), EXIT_DLL_NOT_FOUND);
        assert!(ops.calls.borrow().is_empty(), "no registry writes");
        assert!(!*ops.notify_called.borrow());
    }

    #[test]
    fn install_returns_3_when_thumbnail_clsid_fails() {
        let ops = MockCliOps::new().fail_on("register_clsid:user");
        assert_eq!(run_install(&ops), EXIT_CLSID_FAILED);
        // Aborts before any extension binding, and Explorer is not
        // told to reload a registration that was never written.
        assert_eq!(ops.calls.borrow().len(), 1);
        assert!(!*ops.notify_called.borrow());
    }

    #[test]
    fn install_returns_3_when_preview_clsid_fails() {
        let ops = MockCliOps::new().fail_on("register_preview_clsid:user");
        assert_eq!(run_install(&ops), EXIT_CLSID_FAILED);
        assert_eq!(ops.calls.borrow().len(), 2);
        assert!(!*ops.notify_called.borrow());
    }

    #[test]
    fn install_returns_3_when_preview_handler_list_registration_fails() {
        let ops = MockCliOps::new().fail_on("register_preview_handler_list_entry:user");
        assert_eq!(run_install(&ops), EXIT_CLSID_FAILED);
        assert_eq!(ops.calls.borrow().len(), 3);
        assert!(!*ops.notify_called.borrow());
    }

    #[test]
    fn install_returns_4_when_an_extension_binding_fails() {
        let ext = registry::EXTENSIONS[2];
        let ops = MockCliOps::new().fail_on(&format!("register_extension:user:{ext}"));
        assert_eq!(run_install(&ops), EXIT_EXTENSION_FAILED);
        // Stops at the failing extension: both CLSIDs, preview list, two full
        // extensions before it, then the failing call itself.
        assert_eq!(ops.calls.borrow().len(), 3 + 2 * 2 + 1);
        assert!(!*ops.notify_called.borrow());
    }

    #[test]
    fn install_returns_4_when_a_preview_extension_binding_fails() {
        let ext = registry::EXTENSIONS[0];
        let ops = MockCliOps::new().fail_on(&format!("register_preview_extension:user:{ext}"));
        assert_eq!(run_install(&ops), EXIT_EXTENSION_FAILED);
        assert!(!*ops.notify_called.borrow());
    }

    // ----- run_uninstall --------------------------------------------------

    #[test]
    fn uninstall_cleans_both_hives_and_notifies() {
        let ops = MockCliOps::new();
        assert_eq!(run_uninstall(&ops), EXIT_OK);
        assert!(*ops.notify_called.borrow());

        let calls = ops.calls.borrow();
        // Per scope: thumbnail + preview unbind per extension, then global
        // preview-list and both CLSID removals. Machine first.
        let per_scope = registry::EXTENSIONS.len() * 2 + 3;
        assert_eq!(calls.len(), per_scope * 2);
        assert!(calls[..per_scope].iter().all(|c| c.contains(":machine")));
        assert!(calls[per_scope..].iter().all(|c| c.contains(":user")));
        for scope in ["machine", "user"] {
            assert!(calls.contains(&format!("unregister_clsid:{scope}")));
            assert!(calls.contains(&format!("unregister_preview_clsid:{scope}")));
            assert!(calls.contains(&format!("unregister_preview_handler_list_entry:{scope}")));
            for &ext in registry::EXTENSIONS {
                assert!(calls.contains(&format!("unregister_extension:{scope}:{ext}")));
                assert!(calls.contains(&format!("unregister_preview_extension:{scope}:{ext}")));
            }
        }
    }

    #[test]
    fn uninstall_is_best_effort_and_still_succeeds_on_failures() {
        // Fail every single unregister call — typically AccessDenied
        // on HKLM from a non-elevated uninstaller. The driver must
        // keep going, still notify Explorer, and still exit 0.
        let ops = MockCliOps::new();
        for scope in ["machine", "user"] {
            ops.fail_on
                .borrow_mut()
                .push(format!("unregister_clsid:{scope}"));
            ops.fail_on
                .borrow_mut()
                .push(format!("unregister_preview_clsid:{scope}"));
            ops.fail_on
                .borrow_mut()
                .push(format!("unregister_preview_handler_list_entry:{scope}"));
            for &ext in registry::EXTENSIONS {
                ops.fail_on
                    .borrow_mut()
                    .push(format!("unregister_extension:{scope}:{ext}"));
                ops.fail_on
                    .borrow_mut()
                    .push(format!("unregister_preview_extension:{scope}:{ext}"));
            }
        }
        assert_eq!(run_uninstall(&ops), EXIT_OK);
        let per_scope = registry::EXTENSIONS.len() * 2 + 3;
        assert_eq!(ops.calls.borrow().len(), per_scope * 2, "nothing skipped");
        assert!(*ops.notify_called.borrow());
    }
}
