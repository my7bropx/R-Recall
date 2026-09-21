//! Parser for the `.recall` pack format the built-in knowledge base is
//! authored in, and the embedded packs themselves.
//!
//! ```text
//! %tool nmap
//! %cat command
//! %tags recon
//!
//! @ Full TCP port sweep || nmap -p- -T4 {{target}}
//! ~ scanning
//! ? all ports full range
//! Slow but thorough — pair with -oA to keep the raw output for later greps.
//!
//! @ Ping sweep a subnet || nmap -sn {{subnet}}
//! > nmap
//! ! reason shown only if the heuristic misses this one
//! ```
//!
//! `%` lines set defaults for every `@` block that follows (a later `%` of the
//! same kind replaces it — this is what lets one file cover several tools).
//! Inside a block: `>` overrides the tool, `~` adds tags, `?` adds keywords,
//! `!` forces the danger flag (the reason is folded into the keywords so it's
//! still searchable; most dangerous commands are caught automatically by
//! [`crate::danger::danger_reason`] and never need this). Any other
//! non-empty, non-comment line is appended to the entry's body. `;;` starts a
//! full-line comment.

use std::collections::HashSet;

use crate::danger::danger_reason;
use crate::models::{Category, NewEntry, Source};
use crate::util::slugify;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub source: String,
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.source, self.line, self.message)
    }
}

struct Builder {
    title: String,
    tool: String,
    category: Category,
    tags: Vec<String>,
    command: String,
    keywords: Vec<String>,
    danger: bool,
    body: Vec<String>,
    line: usize,
}

fn finish(b: Builder, source: &str, keys: &mut HashSet<String>, out: &mut Vec<NewEntry>) -> Result<(), ParseError> {
    if b.title.trim().is_empty() {
        return Err(ParseError { source: source.into(), line: b.line, message: "entry has no title".into() });
    }
    let content = b.body.join("\n").trim().to_string();
    let category = if !b.command.trim().is_empty() && b.category == Category::Note {
        Category::Command
    } else {
        b.category
    };
    let auto_danger = danger_reason(&b.command).is_some();
    let base = format!("{}/{}", if b.tool.is_empty() { "misc" } else { &b.tool }, slugify(&b.title));
    let mut key = base.clone();
    let mut n = 2;
    while !keys.insert(key.clone()) {
        key = format!("{}-{}", base, n);
        n += 1;
    }
    out.push(NewEntry {
        title: b.title,
        content,
        category,
        tags: b.tags,
        tool: b.tool,
        command: b.command,
        keywords: b.keywords.join(" "),
        danger: b.danger || auto_danger,
        source: Source::Pack,
        pack_key: key,
    });
    Ok(())
}

/// Parse one pack file. `source` names it for error messages (typically the
/// embedded filename).
pub fn parse(text: &str, source: &str) -> Result<Vec<NewEntry>, ParseError> {
    let mut out = Vec::new();
    let mut keys: HashSet<String> = HashSet::new();

    let mut def_tool = String::new();
    let mut def_cat = Category::Command;
    let mut def_tags: Vec<String> = Vec::new();

    let mut cur: Option<Builder> = None;

    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        let line = raw.trim_end();
        let t = line.trim_start();

        if t.starts_with(";;") {
            continue;
        }
        if let Some(rest) = t.strip_prefix('%') {
            if let Some(b) = cur.take() {
                finish(b, source, &mut keys, &mut out)?;
            }
            let rest = rest.trim();
            let (key, val) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            let val = val.trim();
            match key {
                "tool" => def_tool = val.to_lowercase(),
                "cat" => {
                    def_cat = Category::parse(val).ok_or_else(|| ParseError {
                        source: source.into(),
                        line: lineno,
                        message: format!("unknown %cat '{}'", val),
                    })?
                }
                "tags" => def_tags = split_list(val),
                other => {
                    return Err(ParseError { source: source.into(), line: lineno, message: format!("unknown header '%{}'", other) })
                }
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix('@') {
            if let Some(b) = cur.take() {
                finish(b, source, &mut keys, &mut out)?;
            }
            let (title, command) = match rest.trim().split_once("||") {
                Some((a, c)) => (a.trim().to_string(), c.trim().to_string()),
                None => (rest.trim().to_string(), String::new()),
            };
            cur = Some(Builder {
                title,
                tool: def_tool.clone(),
                category: def_cat.clone(),
                tags: def_tags.clone(),
                command,
                keywords: Vec::new(),
                danger: false,
                body: Vec::new(),
                line: lineno,
            });
            continue;
        }

        let Some(b) = cur.as_mut() else {
            if !t.is_empty() {
                return Err(ParseError { source: source.into(), line: lineno, message: "content before the first '@ Title'".into() });
            }
            continue;
        };

        if let Some(rest) = t.strip_prefix('>') {
            b.tool = rest.trim().to_lowercase();
        } else if let Some(rest) = t.strip_prefix('~') {
            b.tags.extend(split_list(rest.trim()));
        } else if let Some(rest) = t.strip_prefix('?') {
            b.keywords.push(rest.trim().to_string());
        } else if let Some(rest) = t.strip_prefix('!') {
            b.danger = true;
            let reason = rest.trim();
            if !reason.is_empty() {
                b.keywords.push(reason.to_string());
            }
        } else {
            if line.is_empty() && b.body.is_empty() {
                continue; // swallow leading blank lines
            }
            b.body.push(line.to_string());
        }
    }
    if let Some(b) = cur.take() {
        finish(b, source, &mut keys, &mut out)?;
    }
    for e in &mut out {
        while e.content.ends_with('\n') {
            e.content.pop();
        }
    }
    Ok(out)
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty()).collect()
}

// ─── embedded packs ──────────────────────────────────────────────────────────

/// `(filename, text)` for every pack shipped inside the binary.
pub fn embedded() -> Vec<(&'static str, &'static str)> {
    vec![
        ("web.recall", include_str!("packs/web.recall")),
        ("ad.recall", include_str!("packs/ad.recall")),
        ("passwords.recall", include_str!("packs/passwords.recall")),
        ("recon.recall", include_str!("packs/recon.recall")),
        ("exploitation.recall", include_str!("packs/exploitation.recall")),
        ("forensics.recall", include_str!("packs/forensics.recall")),
        ("network.recall", include_str!("packs/network.recall")),
        ("linux.recall", include_str!("packs/linux.recall")),
        ("dev.recall", include_str!("packs/dev.recall")),
        ("wireless.recall", include_str!("packs/wireless.recall")),
    ]
}

/// Parse every embedded pack. Panics on a parse error: the pack ships inside
/// the binary, so a bad file is a build-time bug, not a runtime condition —
/// caught immediately by `builtin_entries_are_well_formed` below.
pub fn builtin_entries() -> Vec<NewEntry> {
    let mut out = Vec::new();
    for (name, text) in embedded() {
        match parse(text, name) {
            Ok(v) => out.extend(v),
            Err(e) => panic!("built-in pack is malformed: {}", e),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
%tool nmap
%cat command
%tags recon

@ Full TCP sweep || nmap -p- -T4 {{target}}
~ scanning
? all ports
Slow but thorough.

Second paragraph survives.

@ Ping sweep || nmap -sn {{subnet}}
> nmap

@ A note, no command
Just prose, category falls back to the file default.
";

    #[test]
    fn parses_headers_body_and_overrides() {
        let v = parse(SAMPLE, "sample").unwrap();
        assert_eq!(v.len(), 3);

        assert_eq!(v[0].title, "Full TCP sweep");
        assert_eq!(v[0].tool, "nmap");
        assert_eq!(v[0].command, "nmap -p- -T4 {{target}}");
        assert_eq!(v[0].tags, vec!["recon", "scanning"], "file tags then entry tags");
        assert_eq!(v[0].keywords, "all ports");
        assert_eq!(v[0].content, "Slow but thorough.\n\nSecond paragraph survives.");
        assert_eq!(v[0].category, Category::Command);
        assert_eq!(v[0].source, Source::Pack);
        assert_eq!(v[0].pack_key, "nmap/full-tcp-sweep");

        assert_eq!(v[1].tool, "nmap", "> re-states the same tool without error");

        assert_eq!(v[2].category, Category::Command, "%cat command is the file default even without a command");
    }

    #[test]
    fn duplicate_titles_get_distinct_pack_keys() {
        let src = "%tool x\n@ Same || a\n@ Same || b\n@ Same || c\n";
        let v = parse(src, "s").unwrap();
        let keys: Vec<&str> = v.iter().map(|e| e.pack_key.as_str()).collect();
        assert_eq!(keys, vec!["x/same", "x/same-2", "x/same-3"]);
    }

    #[test]
    fn danger_is_automatic_and_explicit_reasons_are_searchable() {
        let src = "%tool rm\n@ Wipe a dir || rm -rf {{path}}\n\n%tool git\n@ Force push || git push --force\n! rewrites history, ask first\n";
        let v = parse(src, "s").unwrap();
        assert!(v[0].danger, "caught by the heuristic");
        assert!(v[1].danger);
        assert!(v[1].keywords.contains("rewrites history"));
    }

    #[test]
    fn category_defaults_to_command_only_when_a_command_is_present() {
        let src = "%tool x\n%cat note\n@ Has a command || echo hi\n@ Has none\n";
        let v = parse(src, "s").unwrap();
        assert_eq!(v[0].category, Category::Command, "note default is overridden by having a command");
        assert_eq!(v[1].category, Category::Note);
    }

    #[test]
    fn comments_and_blank_lines_are_not_content() {
        let src = "%tool x\n;; a top comment\n\n@ T || cmd\n;; not content\n\nreal body\n";
        let v = parse(src, "s").unwrap();
        assert_eq!(v[0].content, "real body");
    }

    #[test]
    fn errors_are_positioned() {
        let e = parse("stray text\n", "bad").unwrap_err();
        assert_eq!((e.source.as_str(), e.line), ("bad", 1));
        let e = parse("%cat nonsense\n", "bad").unwrap_err();
        assert!(e.message.contains("cat"));
        let e = parse("%mystery x\n", "bad").unwrap_err();
        assert!(e.message.contains("mystery"));
    }

    #[test]
    fn builtin_entries_are_well_formed() {
        let entries = builtin_entries();
        assert!(entries.len() > 100, "expected a substantial pack, got {}", entries.len());

        let mut keys: HashSet<&str> = HashSet::new();
        for e in &entries {
            assert!(keys.insert(e.pack_key.as_str()), "duplicate pack_key across files: {}", e.pack_key);
            assert!(!e.title.trim().is_empty());
            assert!(!e.tool.trim().is_empty(), "every pack entry should name a tool: {:?}", e.title);
            assert_eq!(e.source, Source::Pack);
            assert!(e.tool.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '.' | '_')), "tool not normalized: {}", e.tool);
        }
    }

    #[test]
    fn builtin_pack_covers_the_measured_gaps() {
        let entries = builtin_entries();
        let tools: HashSet<String> = entries.iter().map(|e| e.tool.clone()).collect();
        for must in [
            "feroxbuster", "wpscan", "netexec", "kerbrute", "certipy-ad", "ligolo-ng", "chisel",
            "evil-winrm", "bloodhound-python", "cargo", "rustup", "ffmpeg", "podman", "nft",
        ] {
            assert!(tools.contains(must), "expected the pack to cover '{}' (measured as missing)", must);
        }
    }
}
