// SPDX-License-Identifier: Apache-2.0
//! macOS FSKit extension readiness probe.
//!
//! Used by the CLI's mount lifecycle to decide whether to attempt
//! an FSKit mount or fall through to the NFS fallback. The probe
//! shells out to `pluginkit -m` and looks for the Heddle bundle
//! identifier; the `+` / `-` prefix indicates enabled / disabled.
//!
//! When the extension is disabled or missing, the caller is
//! expected to print a one-line setup hint and open
//! System Settings to the File System Extensions pane. This
//! module exposes `open_settings()` to do that consistently.

use std::process::Command;

/// Bundle identifier of the embedded FSKit extension. Must match
/// the `PRODUCT_BUNDLE_IDENTIFIER` on the `HeddleFSModule` target
/// in `crates/mount/swift/HeddleHost/HeddleHost.xcodeproj`.
pub const EXTENSION_BUNDLE_ID: &str = "sh.heddle.HeddleHost.HeddleFSModule";

/// What the readiness probe found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    /// The extension is installed and enabled. Mounts via
    /// `mount -t heddle` will succeed.
    Ready,
    /// The host app is installed (extension is discoverable) but
    /// the user hasn't toggled it on. Caller should open System
    /// Settings and ask for one click.
    NeedsApproval,
    /// The extension isn't installed at all. Either the host app
    /// isn't in `/Applications` yet, or this isn't a build with
    /// the System Extension distribution piece. Caller should
    /// fall through to NFS.
    NotInstalled,
    /// `pluginkit` failed for some other reason (older macOS,
    /// missing binary, etc.). Treat as NotInstalled.
    Unknown,
}

/// Probe the system for our FSKit extension's state. Synchronous
/// — `pluginkit` returns in <100ms on a warm system.
pub fn probe() -> Readiness {
    // `pluginkit -m -p com.apple.fskit.fsmodule` lists every
    // registered FSKit extension, one per line. Format:
    //     +    sh.heddle.HeddleHost.HeddleFSModule(1.0)
    //     -    com.example.other(2.0)
    // First char: `+` = enabled, `-` = disabled.
    let output = match Command::new("pluginkit")
        .args(["-m", "-p", "com.apple.fskit.fsmodule"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Readiness::Unknown,
    };
    if !output.status.success() {
        return Readiness::Unknown;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // Lines look like:  "+    sh.heddle.HeddleHost.HeddleFSModule(1.0)"
        let trimmed = line.trim_start();
        if !trimmed.contains(EXTENSION_BUNDLE_ID) {
            continue;
        }
        let enabled = line.trim_start().starts_with('+');
        return if enabled {
            Readiness::Ready
        } else {
            Readiness::NeedsApproval
        };
    }
    Readiness::NotInstalled
}

/// Open System Settings → General → Login Items & Extensions →
/// File System Extensions. Best-effort; logs and returns on
/// failure rather than propagating the error.
pub fn open_settings() {
    let url = "x-apple.systempreferences:com.apple.LoginItems-Settings.extension?Extensions";
    let _ = Command::new("open").arg(url).status();
}

/// One-line setup hint to print to stderr when the extension
/// isn't ready. Kept here so the wording stays consistent across
/// every call site that hits the prompt.
pub fn setup_hint() -> &'static str {
    "Heddle FSKit extension not enabled.\n\
     Opening System Settings — toggle \"Heddle\" on under \
     File System Extensions, then re-run."
}
