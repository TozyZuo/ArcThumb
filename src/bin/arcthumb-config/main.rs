//! ArcThumb Configuration — dual-mode binary.
//!
//! ## GUI mode (default)
//!
//! Running with no arguments launches a Slint-based settings window
//! where the user can enable/disable individual file extensions and
//! tweak the thumbnail selection behaviour (sort order, cover-name
//! preference).
//!
//! ## CLI mode
//!
//! ```text
//! arcthumb-config.exe --install
//!     Write the full shell-extension registration. Hive is picked
//!     automatically by elevation: HKLM when the process is elevated
//!     (per-machine install), HKCU otherwise (per-user install).
//!     Called by the Inno Setup installer as a post-install step.
//!
//! arcthumb-config.exe --uninstall
//!     Remove every ShellEx binding and the CLSID key from BOTH
//!     hives (best effort) so a per-user → per-machine switch or
//!     vice versa doesn't leave stale entries behind.
//!     Called by the uninstaller as a pre-uninstall step.
//! ```
//!
//! Exit codes:
//! - `0` success
//! - `2` DLL not found (for --install)
//! - `3` CLSID registration failed
//! - `4` extension binding failed
//! - `5` GUI init failed (very rare)

// Hide the console on release builds. Debug builds keep the console
// so `cargo run` output is visible.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod apply;
mod cache;
mod cli;
mod dialogs;
mod dll_path;
mod extension_model;
mod locale;
mod message_box;
mod state;
mod ui;
mod update;
mod update_check;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("--install") => std::process::exit(cli::run_install(&cli::RealCliOps)),
        Some("--uninstall") => std::process::exit(cli::run_uninstall(&cli::RealCliOps)),
        _ => {
            // Surface the failure with a native MessageBox before
            // exiting. Release builds run as `windows_subsystem =
            // "windows"`, so without this the user sees nothing —
            // not even a console line — and reports the binary as
            // broken. Reported in microsoft/winget-pkgs#364519.
            if let Err(e) = ui::run_gui() {
                let strings = locale::current();
                message_box::error(
                    strings.error_title,
                    &format!("{}\n\n{e}", strings.error_gui_init),
                );
                std::process::exit(5);
            }
        }
    }
}
