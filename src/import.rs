//! Heading-aware Markdown importer.
//!
//! Turns a Markdown file into recall entries. The rules were derived from real,
//! messy notes files:
//!   * Real headings are 2–4 hashes (`##`, `###`, `####`). A single `#` is a
//!     shell comment or a decorative banner and never starts an entry — this is
//!     what keeps inline `# comment` lines inside code blocks from being treated
//!     as headings.
//!   * An entry's body runs from just after its heading to the next heading of
//!     any level in {2,3,4}. Headings with no direct body (pure containers) are
//!     dropped; their children become the entries.
//!   * The major category comes from the known single-`#` section banners.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    danger::danger_reason,
    db::Database,
    derive::derive_tool,
    models::{fenced_blocks, Category, NewEntry, Source},
};

// The eight top-level banners -> short slug used as the primary tag / context.
const BANNERS: &[(&str, &str)] = &[
    ("LINUX & SYSTEM ADMINISTRATION", "linux"),
    ("GIT & GITHUB", "git"),
    ("TMUX", "tmux"),
    ("VIM / NEOVIM", "vim"),
    ("CLI CHEATSHEETS & REFERENCE", "cli"),
    ("ENCRYPTION & SECURITY", "crypto"),
    ("WINDOWS", "windows"),
    ("MISCELLANEOUS", "misc"),
];

const NAV_DROP: &[&str] = &[
    "table of contents", "quick navigation", "print this", "tags",
    "quick reference card",
];
const SOFT_DROP: &[&str] = &["overview"];
const GENERIC: &[&str] = &[
    "examples", "use when", "best practices", "troubleshooting", "configuration",
    "installation", "prerequisites", "conclusion", "quick reference",
    "quick reference commands", "core concepts", "mental model", "syntax",
    "description", "advanced features", "security hardening", "key learning points",
    "additional resources", "additional tools", "advanced configuration",
    "pro tips", "tips & tricks",
];
const CMD_PREFIXES: &[&str] = &[
    "sudo ", "nmap ", "grep ", "sed ", "awk ", "git ", "ssh ", "curl ", "wget ",
    "cat ", "ls ", "cd ", "chmod ", "chown ", "ip ", "systemctl ",
];

pub struct ImportOptions {
    pub dry_run:      bool,
    pub flagged_only: bool,
    pub extra_tags:   Vec<String>,
}

pub struct ImportStats {
    pub candidates: usize,  // entries that passed all content filters
    pub skipped:    usize,  // headings dropped (nav/empty/flag filter)
    pub duplicates: usize,  // entries skipped because identical data already exists
}

// ─── line classification ───────────────────────────────────────────────────

/// A 2–4 hash heading. Returns (level, title-without-hashes).
fn parse_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|&c| c == '#').count();
    if !(2..=4).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..]; // '#' is 1 byte, so byte index == count
    if !rest.starts_with(' ') {
        return None;
    }
    let title = rest.trim().trim_end_matches('#').trim();
    if title.is_empty() {
        None
    } else {
        Some((hashes, title))
    }
}

/// A single-`#` line that matches one of the known banners -> its slug.
fn parse_banner(line: &str) -> Option<&'static str> {
    if line.chars().take_while(|&c| c == '#').count() != 1 {
        return None;
    }
    let text = line[1..].replace("**", "");
    let text = text.trim();
    BANNERS
        .iter()
        .find(|(name, _)| *name == text)
        .map(|(_, slug)| *slug)
}

// ─── text helpers ────────────────────────────────────────────────────────────

fn strip_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn strip_leading_symbols(s: &str) -> String {
    s.trim_start_matches(|c: char| !c.is_alphanumeric() && c != '(')
        .to_string()
}

/// Strip list numbering like `1) `, `2. `, `3: `, or the keycap form `1️⃣ `.
/// A bare number followed only by a space (e.g. "5 things") is left intact.
fn strip_leading_numbering(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.first().map_or(true, |c| !c.is_ascii_digit()) {
        return s.to_string();
    }
    let mut i = 0;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    let mut j = i;
    let mut saw_sep = false;
    while j < chars.len()
        && matches!(chars[j], '\u{FE0F}' | '\u{20E3}' | ')' | '.' | ':')
    {
        j += 1;
        saw_sep = true;
    }
    if !saw_sep {
        return s.to_string(); // "2FA", "7z", "5 things" -> unchanged
    }
    while j < chars.len() && chars[j].is_whitespace() {
        j += 1;
    }
    chars[j..].iter().collect()
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_title(raw: &str) -> String {
    let mut t = strip_html(raw).replace("**", "");
    t = t.trim().trim_matches('`').trim().to_string();
    t = strip_leading_symbols(&t);
    t = strip_leading_numbering(&t);
    t = strip_leading_symbols(&t); // numbering may expose more leading symbols
    let t = t.trim().trim_end_matches(':').trim().to_string();
    collapse_ws(&t)
}

fn slug(s: &str, max: usize) -> String {
    let cleaned = strip_html(s).replace("**", "");
    let mut out = String::new();
    for c in cleaned.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if c == ' ' || c == '-' || c == '_' {
            out.push('-');
        }
    }
    // collapse consecutive dashes
    let mut collapsed = String::new();
    let mut prev_dash = false;
    for c in out.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    let chars: Vec<char> = collapsed.trim_matches('-').chars().collect();
    if chars.len() <= max {
        return chars.iter().collect();
    }
    let mut cut = chars[..max].to_vec();
    if let Some(pos) = cut.iter().rposition(|&c| c == '-') {
        if pos >= 8 {
            cut.truncate(pos); // trim to a word boundary, not mid-word
        }
    }
    cut.iter().collect::<String>().trim_matches('-').to_string()
}

fn strip_trailing_hr(body: &str) -> String {
    let mut lines: Vec<&str> = body.split('\n').collect();
    while let Some(last) = lines.last() {
        let t = last.trim();
        let is_hr = t.len() >= 3
            && (t.chars().all(|c| c == '-')
                || t.chars().all(|c| c == '*')
                || t.chars().all(|c| c == '_'));
        if t.is_empty() || is_hr {
            lines.pop();
        } else {
            break;
        }
    }
    lines.join("\n")
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{}\n\n… [truncated — full text in the source file]", cut.trim_end())
}

fn infer_category(title_low: &str, body: &str) -> Category {
    let has_code = body.contains("```");
    let tool_word = ["tool", "framework", "suite", "install"]
        .iter()
        .any(|w| title_low.contains(w));
    if tool_word && !has_code {
        return Category::Tool;
    }
    let command_line = body.lines().any(|l| {
        let t = l.trim_start();
        CMD_PREFIXES.iter().any(|p| t.starts_with(p))
    });
    if has_code || command_line {
        Category::Command
    } else {
        Category::Note
    }
}

// ─── main entry point ─────────────────────────────────────────────────────────

pub fn import_markdown(db: &mut Database, path: &Path, opts: &ImportOptions) -> Result<ImportStats> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let lines: Vec<&str> = text.split('\n').collect();

    struct H {
        idx:   usize,
        level: usize,
        raw:   String,
    }
    let mut heads: Vec<H> = Vec::new();
    let mut banners: Vec<(usize, &'static str)> = Vec::new();
    for (i, ln) in lines.iter().enumerate() {
        if let Some((lvl, title)) = parse_heading(ln) {
            heads.push(H { idx: i, level: lvl, raw: title.to_string() });
        } else if let Some(bslug) = parse_banner(ln) {
            banners.push((i, bslug));
        }
    }

    let major_at = |idx: usize| -> &'static str {
        let mut cur = "linux"; // the file opens with Linux admin before any banner
        for &(bidx, bslug) in &banners {
            if bidx <= idx {
                cur = bslug;
            } else {
                break;
            }
        }
        cur
    };

    let mut items: Vec<NewEntry> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut skipped = 0usize;

    for (h, head) in heads.iter().enumerate() {
        let end = if h + 1 < heads.len() { heads[h + 1].idx } else { lines.len() };
        let body = lines[head.idx + 1..end].join("\n");
        let body = strip_trailing_hr(body.trim());
        let body = body.trim().to_string();

        let title = clean_title(&head.raw);
        if title.is_empty() {
            skipped += 1;
            continue;
        }
        let low = title.to_lowercase();

        // nearest section = most recent level-2 heading at or before this one
        let mut section: Option<String> = None;
        for j in (0..=h).rev() {
            if heads[j].level == 2 {
                section = Some(clean_title(&heads[j].raw));
                break;
            }
        }
        let major = major_at(head.idx);

        let raw_up = head.raw.to_uppercase();
        let section_flag = section
            .as_ref()
            .map_or(false, |s| {
                let u = s.to_uppercase();
                u.contains("CRITICAL") || u.contains("IMPORTANT")
            });
        let is_flagged =
            raw_up.contains("CRITICAL") || raw_up.contains("IMPORTANT") || section_flag;
        if opts.flagged_only && !is_flagged {
            skipped += 1;
            continue;
        }

        if NAV_DROP.contains(&low.as_str()) {
            skipped += 1;
            continue;
        }
        if SOFT_DROP.contains(&low.as_str()) && body.len() < 400 {
            skipped += 1;
            continue;
        }
        if body.len() < 20 {
            skipped += 1;
            continue;
        }

        let category = infer_category(&low, &body);

        // tags: major, section slug, flag, plus any user-supplied tags
        let mut tags: Vec<String> = vec![major.to_string()];
        if let Some(ref s) = section {
            let ss = slug(s, 24);
            if !ss.is_empty() && ss != major {
                tags.push(ss);
            }
        }
        if raw_up.contains("CRITICAL") {
            tags.push("critical".into());
        } else if raw_up.contains("IMPORTANT") {
            tags.push("important".into());
        }
        tags.extend(opts.extra_tags.iter().cloned());
        let mut seen_tag = HashSet::new();
        tags.retain(|t| !t.is_empty() && seen_tag.insert(t.clone()));
        tags.truncate(5);

        // readable, mostly-unique title
        let generic = GENERIC.contains(&low.as_str());
        let mut disp = title.clone();
        if generic || title.chars().count() < 8 {
            if let Some(ref s) = section {
                if slug(s, 24) != slug(&title, 24) {
                    disp = format!("{} — {}", s, title);
                }
            }
        }
        let count = seen.entry(disp.to_lowercase()).or_insert(0);
        *count += 1;
        if *count > 1 {
            disp = format!("{} ({})", disp, *count);
        }

        let content = cap_chars(&body, 20000);
        let tool = derive_tool(&disp, &content, &tags);
        let command = if category == Category::Command {
            fenced_blocks(&content).into_iter().find(|b| !b.trim().is_empty()).unwrap_or_default()
        } else {
            String::new()
        };
        let danger = danger_reason(&command).is_some();
        items.push(NewEntry {
            title: disp,
            content,
            category,
            tags,
            tool,
            command,
            danger,
            source: Source::Import,
            ..Default::default()
        });
    }

    // De-duplicate: never import data that already exists in the database, and
    // never import the same (title, content) twice within a single run. This
    // makes re-importing the same file a no-op.
    let mut existing = db.existing_title_content()?;
    let mut unique: Vec<NewEntry> = Vec::with_capacity(items.len());
    let mut duplicates = 0usize;
    for e in items {
        let key = (e.title.clone(), e.content.clone());
        if existing.contains(&key) {
            duplicates += 1;
        } else {
            existing.insert(key);
            unique.push(e);
        }
    }

    let candidates = unique.len();
    if !opts.dry_run {
        db.add_entries(&unique)?;
    }
    Ok(ImportStats { candidates, skipped, duplicates })
}
