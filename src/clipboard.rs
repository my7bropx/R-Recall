//! System clipboard, shared by the TUI and the CLI (`copy`/`pick`).

use std::io::Write;
use std::process::{Command, Stdio};

/// Tools tried in order, first found wins.
const TOOLS: &[(&str, &[&str])] =
    &[("xclip", &["-selection", "clipboard"]), ("xsel", &["--clipboard", "--input"]), ("wl-copy", &[])];

/// Write `text` verbatim to the system clipboard (no trailing newline added).
/// Returns the tool that worked, or `None` if none of them did.
pub fn copy(text: &str) -> Option<&'static str> {
    for (tool, args) in TOOLS {
        // stdout/stderr must NOT be inherited: xclip forks a background process
        // to keep serving the X selection after this call returns, and an
        // inherited stdout/stderr would leave that process attached to our
        // (possibly raw-mode) terminal.
        let spawned =
            Command::new(tool).args(*args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        if let Ok(mut child) = spawned {
            let wrote = child.stdin.take().map(|mut s| s.write_all(text.as_bytes()).is_ok()).unwrap_or(false);
            child.wait().ok(); // always reap, even on a failed write
            if wrote {
                return Some(tool);
            }
        }
    }
    None
}

/// Human-readable hint for when none of the clipboard tools are installed.
pub const MISSING_HINT: &str = "install xclip, xsel, or wl-copy for clipboard support";
