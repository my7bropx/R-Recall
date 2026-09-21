//! Small shared helpers: hashing, JSON escaping, text utilities, terminal
//! detection and ANSI painting for the plain-text CLI output.

use std::io::IsTerminal;

// ─── hashing ─────────────────────────────────────────────────────────────────

/// 64-bit FNV-1a over a list of fields. Stable across runs and platforms, which
/// is what the pack sync relies on to tell "unchanged" from "edited by the user".
pub fn fnv64(parts: &[&str]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut h = OFFSET;
    for p in parts {
        for b in p.as_bytes() {
            h ^= *b as u64;
            h = h.wrapping_mul(PRIME);
        }
        // field separator, so ("ab","c") and ("a","bc") differ
        h ^= 0x1f;
        h = h.wrapping_mul(PRIME);
    }
    h
}

pub fn fnv_hex(parts: &[&str]) -> String {
    format!("{:016x}", fnv64(parts))
}

// ─── text ────────────────────────────────────────────────────────────────────

/// Shorten to `n` characters, ending in `…` when cut.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Lowercase alphanumeric slug with single dashes: "Scan ALL ports!" → "scan-all-ports".
pub fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut dash = true; // swallow leading separators
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// Collapse every run of whitespace to one space.
pub fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Escape a string for a `LIKE ... ESCAPE '\'` pattern.
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ─── JSON (just enough for `--json` and `export`; no serde dependency) ───────

pub fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn json_arr(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| json_str(s)).collect();
    format!("[{}]", inner.join(","))
}

// ─── terminal ────────────────────────────────────────────────────────────────

pub fn stdout_is_tty() -> bool {
    std::io::stdout().is_terminal()
}
pub fn stdin_is_tty() -> bool {
    std::io::stdin().is_terminal()
}
pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// ANSI painter for CLI output. Disabled automatically when the target is not a
/// terminal or `NO_COLOR` is set, so piped output stays clean.
#[derive(Clone, Copy)]
pub struct Paint {
    pub on: bool,
}

impl Paint {
    pub fn for_stdout() -> Self {
        Paint { on: stdout_is_tty() && std::env::var_os("NO_COLOR").is_none() }
    }
    pub fn for_stderr() -> Self {
        Paint { on: stderr_is_tty() && std::env::var_os("NO_COLOR").is_none() }
    }
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{}m{}\x1b[0m", code, s)
        } else {
            s.to_string()
        }
    }
    pub fn bold(&self, s: &str) -> String { self.wrap("1", s) }
    pub fn dim(&self, s: &str) -> String { self.wrap("2", s) }
    pub fn red(&self, s: &str) -> String { self.wrap("31", s) }
    pub fn green(&self, s: &str) -> String { self.wrap("32", s) }
    pub fn yellow(&self, s: &str) -> String { self.wrap("33", s) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv_is_deterministic_and_field_aware() {
        assert_eq!(fnv64(&["a", "b"]), fnv64(&["a", "b"]));
        assert_ne!(fnv64(&["ab", "c"]), fnv64(&["a", "bc"]), "field boundaries matter");
    }

    #[test]
    fn slugify_basics() {
        assert_eq!(slugify("Scan ALL ports!"), "scan-all-ports");
        assert_eq!(slugify("  --weird__title--  "), "weird-title");
        assert_eq!(slugify("Kerberoast (GetUserSPNs.py)"), "kerberoast-getuserspns-py");
        assert_eq!(slugify(""), "");
    }

    #[test]
    fn json_escapes_control_and_quotes() {
        assert_eq!(json_str("a\"b\\c\nd\t\u{1}"), "\"a\\\"b\\\\c\\nd\\t\\u0001\"");
        assert_eq!(json_arr(&["x".into(), "y".into()]), "[\"x\",\"y\"]");
    }

    #[test]
    fn truncate_counts_chars_not_bytes() {
        assert_eq!(truncate("héllo wörld", 20), "héllo wörld");
        assert_eq!(truncate("héllo wörld", 6), "héllo…");
    }

    #[test]
    fn like_escape_protects_wildcards() {
        assert_eq!(like_escape("50%_off\\"), "50\\%\\_off\\\\");
    }
}
