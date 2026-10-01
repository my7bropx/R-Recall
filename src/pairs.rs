//! One entry per command.
//!
//! Cheat sheets annotate each command with what it does: a `# comment` above
//! it, `cmd   # comment` after it, a `` - `cmd` - description `` bullet, a
//! table row, or a two-column key table (`dd   delete line`). Those words are
//! exactly what a person types when searching, so every (description, command)
//! pair is worth an entry of its own: the title says what it does and `command`
//! is that one command — not the whole section it happens to live in.
//!
//! This module also decides which fenced blocks *are* command blocks, so the
//! importer's category and the pairs it extracts can never disagree.

use crate::derive::{self, Vocab};
use crate::models::fenced_blocks_lang;

/// A comment above more lines than this describes a script, not one command.
const MAX_GROUP_LINES: usize = 6;
/// Per-section cap, so a pasted dotfile cannot flood the database.
const MAX_PAIRS: usize = 80;

#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    /// What it does — becomes the entry title.
    pub desc:    String,
    /// What to type: one command, or a short group of lines that belong together.
    pub command: String,
    pub kind:    PairKind,
    /// The program the block itself names (`vim`/`tmux`/`awk` fences, key tables in a
    /// vim/tmux section). `dd` in a vim key table is a keystroke, never the `dd` program.
    pub tool:    Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairKind {
    /// From an annotated shell block.
    Shell,
    /// From a `` - `cmd` - description `` bullet or a table row.
    Inline,
    /// A keystroke or ex-command from a vim/tmux key table.
    Keys,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// Commands to type into a shell.
    Shell,
    /// A key table: keystrokes with a description (vim, tmux, emacs).
    Keys,
    /// Code in another language, config, prose in a fence — not a command.
    Other,
}

// ─── which fenced blocks are commands ────────────────────────────────────────

pub fn is_powershell(lang: &str) -> bool {
    matches!(lang, "powershell" | "ps1" | "pwsh" | "cmd")
}

/// A line that would be typed: not blank, not a comment, and — in a Unix shell
/// block — not a capitalised config directive (`Host myserver`).
pub fn is_command_line(l: &str, unix: bool) -> bool {
    let t = l.trim();
    if t.is_empty() || t.starts_with('#') || matches!(t, "{" | "}") {
        return false;
    }
    !(unix && t.starts_with(|c: char| c.is_ascii_uppercase())) || is_assignment(t)
}

/// `LD_PRELOAD=/tmp/x.so cmd`, `PATH=$PATH:/opt/bin` — upper-case, but typed at a prompt.
fn is_assignment(t: &str) -> bool {
    let name = t.bytes().take_while(|b| b.is_ascii_alphanumeric() || *b == b'_').count();
    name > 0 && t.as_bytes().get(name) == Some(&b'=')
}

/// Decide what a fenced block is.
///
/// * `vim` fences are key tables; `tmux` and `awk` fences are shell-like.
/// * A fence the author labelled as shell is a command block if any line
///   would be typed — so `cd ~` and `source ~/.bashrc` count, while an ssh
///   config (`Host …`, `Port 22`) or a block of only comments does not.
/// * A bare fence inside a vim/tmux section is a key table when its lines are
///   two columns; otherwise it is believed only when its lines read as commands.
pub fn classify(lang: &str, text: &str, vocab: &Vocab, keys_tool: Option<&str>) -> BlockKind {
    match lang {
        "vim" => return BlockKind::Keys,
        "tmux" | "awk" => return BlockKind::Shell,
        _ => {}
    }
    if !derive::is_shell_lang(lang) {
        return BlockKind::Other; // python, lua, yaml, regex…
    }
    let explicit = !lang.is_empty() && !matches!(lang, "text" | "txt");
    if explicit {
        let unix = !is_powershell(lang);
        return if text.lines().any(|l| is_command_line(l, unix)) { BlockKind::Shell } else { BlockKind::Other };
    }
    // Inside a vim/tmux section a two-column fence is a key table first: `dd   delete line`
    // would otherwise read as the `dd` program with arguments.
    if keys_tool.is_some() && looks_like_key_table(text) {
        BlockKind::Keys
    } else if text.lines().any(|l| vocab.command_line(l).is_some()) {
        BlockKind::Shell
    } else {
        BlockKind::Other
    }
}

// ─── small text helpers ──────────────────────────────────────────────────────

const BOX: &[char] = &[
    '┃', '┏', '┓', '┗', '┛', '━', '┣', '┫', '┳', '┻', '╋', '│', '─', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴',
    '┼', '║', '═', '╔', '╗', '╚', '╝',
];

/// `┃  Ctrl-b d      Detach  ┃` -> `Ctrl-b d      Detach`
fn strip_box(line: &str) -> String {
    line.trim_matches(|c: char| BOX.contains(&c) || c.is_whitespace()).to_string()
}

/// Split a two-column line: on `→` / `->` / `=>`, or a run of two spaces (or a tab).
fn split_gap(t: &str) -> Option<(String, String)> {
    for sep in ["→", "->", "=>"] {
        if let Some(i) = t.find(sep) {
            let (l, r) = (t[..i].trim(), t[i + sep.len()..].trim());
            if !l.is_empty() && !r.is_empty() {
                return Some((l.to_string(), r.to_string()));
            }
        }
    }
    let b = t.as_bytes();
    for i in 1..b.len() {
        if b[i] == b'\t' || (b[i] == b' ' && b[i - 1] == b' ') || (b[i] == b' ' && b.get(i + 1) == Some(&b' ')) {
            let (l, r) = (t[..i].trim(), t[i..].trim());
            return (!l.is_empty() && !r.is_empty()).then(|| (l.to_string(), r.to_string()));
        }
    }
    None
}

/// Is the left column plausibly a keystroke or a command, rather than a header
/// word ("Tool", "Action")? Short, symbolic, a chord, or starting with a known program.
fn key_like(left: &str, vocab: &Vocab) -> bool {
    let first = left.split_whitespace().next().unwrap_or("");
    left.chars().count() <= 40
        && (left.chars().count() <= 3
            || left.chars().any(|c| !c.is_alphanumeric() && !c.is_whitespace())
            || vocab.knows(&first.to_lowercase())
            || ["ctrl", "alt", "shift", "meta", "prefix", "leader", "esc", "enter", "tab", "space"]
                .iter()
                .any(|k| first.to_lowercase().starts_with(k)))
}

fn looks_like_key_table(text: &str) -> bool {
    let rows: Vec<String> = text.lines().map(strip_box).filter(|l| !l.is_empty()).collect();
    let hits = rows.iter().filter(|r| split_gap(r).is_some()).count();
    rows.len() >= 2 && hits >= 2 && hits * 10 >= rows.len() * 7
}

fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Markdown and trailing punctuation off, one line, at most 90 chars, capitalised.
pub fn clean_desc(s: &str) -> String {
    let t = s.replace("**", "").replace('`', "");
    let t = t.trim().trim_end_matches([':', '.', ';', ',']).trim();
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = if t.chars().count() > 90 {
        let cut: String = t.chars().take(90).collect();
        cut.rsplit_once(' ').map_or(cut.clone(), |(a, _)| a.to_string())
    } else {
        t
    };
    cap_first(&t)
}

const NOISE: &[&str] = &[
    "example", "examples", "output", "result", "results", "note", "notes", "usage", "syntax", "default",
    "optional", "required", "etc", "todo", "warning", "important", "tip", "same", "see above", "see below",
];

/// Is `d` a description worth a title: real words, not a section marker or an annotation?
fn valid_desc(d: &str, min_words: usize) -> bool {
    let lower = d.to_lowercase();
    d.split_whitespace().count() >= min_words
        && d.chars().filter(|c| c.is_alphabetic()).count() >= 4
        && !d.contains("===")
        && !NOISE.contains(&lower.as_str())
        && !lower.starts_with("output")
        && !lower.starts_with("result")
        && !lower.starts_with("example output")
}

fn leading_desc(c: &str) -> Option<String> {
    let d = clean_desc(c);
    valid_desc(&d, 2).then_some(d) // one word above a group is a section label, not a description
}

/// A description after the command. Two words in a shell or bullet context — a lone word
/// (`# Graceful`) is an annotation, not a title; one is enough for a key (`Ctrl-b d` — Detach).
fn trailing_desc(c: &str, min_words: usize) -> Option<String> {
    let d = clean_desc(c);
    valid_desc(&d, min_words).then_some(d)
}

// ─── comment-annotated lines ─────────────────────────────────────────────────

struct Style {
    /// Characters that start a comment.
    markers: &'static [char],
    /// Also read `keys   description` (two spaces / tab / arrow) as a pair.
    gap:     bool,
}

const SHELL: Style = Style { markers: &['#'], gap: false };
const VIM: Style = Style { markers: &['"', '#'], gap: true };
const KEYS: Style = Style { markers: &['#'], gap: true };

/// `# text` (or `" text` in vim) on a line by itself -> the text. `#!shebang` -> "".
fn full_line_comment<'a>(t: &'a str, markers: &[char]) -> Option<&'a str> {
    let c = t.chars().next()?;
    if !markers.contains(&c) {
        return None;
    }
    let rest = &t[c.len_utf8()..];
    if c == '#' {
        return Some(if rest.starts_with('!') { "" } else { rest.trim_start_matches('#').trim() });
    }
    (rest.is_empty() || rest.starts_with(char::is_whitespace)).then(|| rest.trim())
}

/// `cmd   # text` -> (cmd, Some(text)). A marker only starts a comment after
/// whitespace and before whitespace, and never inside quotes.
fn split_trailing_comment(t: &str, markers: &[char], track_quotes: bool) -> (String, Option<String>) {
    let chars: Vec<(usize, char)> = t.char_indices().collect();
    let mut quote: Option<char> = None;
    for (k, &(i, c)) in chars.iter().enumerate() {
        if track_quotes {
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                continue;
            }
            if c == '\'' || c == '"' {
                quote = Some(c);
                continue;
            }
        }
        if markers.contains(&c)
            && k > 0
            && chars[k - 1].1.is_whitespace()
            && chars.get(k + 1).map_or(true, |&(_, n)| n.is_whitespace())
        {
            return (t[..i].trim_end().to_string(), Some(t[i + c.len_utf8()..].trim().to_string()));
        }
    }
    (t.to_string(), None)
}

fn flush(desc: &Option<String>, group: &mut Vec<String>, kind: PairKind, out: &mut Vec<Pair>) {
    if let Some(d) = desc {
        if !group.is_empty() && group.len() <= MAX_GROUP_LINES {
            out.push(Pair { desc: d.clone(), command: group.join("\n"), kind, tool: None });
        }
    }
    group.clear();
}

fn annotated(text: &str, style: &Style, kind: PairKind, vocab: &Vocab, unix: bool) -> Vec<Pair> {
    let mut out = Vec::new();
    let mut desc: Option<String> = None;
    let mut group: Vec<String> = Vec::new();
    for raw in text.lines() {
        let t = if style.gap { strip_box(raw) } else { raw.trim().to_string() };
        if t.is_empty() {
            flush(&desc, &mut group, kind, &mut out);
            desc = None;
            continue;
        }
        if let Some(c) = full_line_comment(&t, style.markers) {
            flush(&desc, &mut group, kind, &mut out);
            desc = leading_desc(c);
            continue;
        }
        let track = !style.markers.contains(&'"');
        let min_words = if kind == PairKind::Keys { 1 } else { 2 };
        let (code, trailing) = split_trailing_comment(&t, style.markers, track);
        // In a Unix shell block a capitalised line is a config directive (`Host myserver`), not a command.
        if kind == PairKind::Shell && !is_command_line(&code, unix) {
            flush(&desc, &mut group, kind, &mut out);
            desc = None;
            continue;
        }
        if let Some(d) = trailing.as_deref().and_then(|c| trailing_desc(c, min_words)) {
            flush(&desc, &mut group, kind, &mut out);
            out.push(Pair { desc: d, command: code, kind, tool: None });
            desc = None;
            continue;
        }
        if style.gap {
            if let Some((l, r)) = split_gap(&t) {
                if let Some(d) = trailing_desc(&r, min_words).filter(|_| key_like(&l, vocab)) {
                    flush(&desc, &mut group, kind, &mut out);
                    out.push(Pair { desc: d, command: l, kind, tool: None });
                    desc = None;
                    continue;
                }
            }
        }
        if desc.is_some() {
            group.push(code);
        }
    }
    flush(&desc, &mut group, kind, &mut out);
    out
}

// ─── bullets and tables ──────────────────────────────────────────────────────

/// `` - `cmd` - description `` (also `*`, `+`, `1.`; separators `-` `–` `—` `:` `=` `→` `->`).
fn bullet_pair(l: &str) -> Option<(String, String)> {
    let t = l.trim_start();
    let after = match t.strip_prefix(['-', '*', '+']) {
        Some(r) if r.starts_with(char::is_whitespace) => r.trim_start(),
        Some(_) => return None,
        None => {
            let digits = t.bytes().take_while(|b| b.is_ascii_digit()).count();
            let r = t[digits..].strip_prefix(['.', ')'])?;
            if digits == 0 || !r.starts_with(char::is_whitespace) {
                return None;
            }
            r.trim_start()
        }
    };
    let inner = after.strip_prefix('`')?;
    let close = inner.find('`')?;
    let code = inner[..close].trim();
    if code.is_empty() || code.chars().count() > 100 {
        return None;
    }
    let rest = inner[close + 1..].trim_start();
    let desc = ["->", "=>", "—", "–", "→", ":", "="]
        .iter()
        .find_map(|s| rest.strip_prefix(*s))
        .or_else(|| rest.strip_prefix("- "))?
        .trim();
    (!desc.is_empty()).then(|| (code.to_string(), desc.to_string()))
}

fn is_code_cell(c: &str) -> bool {
    c.len() >= 3 && c.starts_with('`') && c.ends_with('`') && !c[1..c.len() - 1].contains('`')
}

/// `` | `cmd` | description | `` or `` | description | `cmd` | … ``
fn table_pair(l: &str) -> Option<(String, String)> {
    let t = l.trim();
    if !t.starts_with('|') {
        return None;
    }
    let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
    if cells.len() < 2 || cells.iter().all(|c| c.chars().all(|ch| matches!(ch, '-' | ':' | ' '))) {
        return None;
    }
    let i = cells.iter().position(|c| is_code_cell(c))?;
    let code = cells[i][1..cells[i].len() - 1].trim();
    let desc = if i == 0 { cells[1] } else { cells[i - 1] };
    (!code.is_empty() && !desc.is_empty() && !is_code_cell(desc)).then(|| (code.to_string(), desc.to_string()))
}

/// Bullet and table pairs outside code fences. `keys` says the section is about a
/// keys tool (vim/tmux/emacs), where a one-word description ("Paste") is fine.
pub fn inline_pairs(body: &str, keys: bool) -> Vec<Pair> {
    let min_words = if keys { 1 } else { 2 };
    let mut out = Vec::new();
    let mut in_code = false;
    for l in body.lines() {
        if l.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if let Some((code, desc)) = bullet_pair(l).or_else(|| table_pair(l)) {
            if let Some(d) = trailing_desc(&desc, min_words) {
                out.push(Pair { desc: d, command: code, kind: PairKind::Inline, tool: None });
            }
        }
    }
    out
}

// ─── the whole section ───────────────────────────────────────────────────────

fn is_option_only(p: &Pair) -> bool {
    let c = p.command.trim();
    c.starts_with('-') && !(p.kind == PairKind::Keys && c.chars().count() == 1)
}

/// Cheat sheets for regex, globs and paths list *notation* in the command column
/// (`[a-zA-Z]{2,}`, `*.log`, `/var/`, `2>&1`, `$0`) — describable, but nothing you would
/// type at a prompt. Keystrokes may look like anything; everything else has to start
/// like a command, or be a path to a program.
fn is_not_a_command(p: &Pair) -> bool {
    let c = p.command.trim();
    if c.ends_with('=') {
        return true; // a dangling option (`conv=`)
    }
    if p.kind == PairKind::Keys {
        return false;
    }
    let first = c.split_whitespace().next().unwrap_or("");
    // A lone absolute path is a "what lives where" table row (`/etc/passwd` — System users),
    // not something to run — unless it is a script.
    let lone = c.split_whitespace().count() == 1;
    let script = [".sh", ".bash", ".py", ".pl", ".rb", ".php"].iter().any(|e| first.ends_with(e));
    if lone && first.starts_with('/') && !script {
        return true;
    }
    let path = (first.starts_with("./") || first.starts_with("~/") || first.starts_with('/'))
        && !first.ends_with('/')
        && first.len() > 2;
    let ok = first.starts_with(|ch: char| ch.is_ascii_alphabetic())
        || path
        || (first.starts_with('!') && first.len() >= 2) // history expansion: !! !$
        || (first.starts_with(':') && first.len() >= 2); // ex command
    !ok
}

/// Every (description, command) pair in a section body. `keys_tool` is the
/// program the section is about when it is a keys topic (`vim`, `tmux`, `emacs`),
/// which is what lets a bare two-column fence be read as a key table.
pub fn extract(body: &str, vocab: &Vocab, keys_tool: Option<&str>) -> Vec<Pair> {
    let mut out: Vec<Pair> = Vec::new();
    for (lang, block) in fenced_blocks_lang(body) {
        let text = block.trim_matches('\n');
        if text.trim().is_empty() {
            continue;
        }
        let kind = classify(&lang, text, vocab, keys_tool);
        let unix = !is_powershell(&lang);
        let mut found = match kind {
            BlockKind::Shell => annotated(text, &SHELL, PairKind::Shell, vocab, unix),
            BlockKind::Keys => annotated(text, if lang == "vim" { &VIM } else { &KEYS }, PairKind::Keys, vocab, unix),
            BlockKind::Other => continue,
        };
        let block_tool = match lang.as_str() {
            "vim" | "tmux" | "awk" => Some(lang.clone()),
            _ if kind == BlockKind::Keys => keys_tool.map(str::to_string),
            _ => None,
        };
        for p in &mut found {
            p.tool = block_tool.clone();
        }
        out.extend(found);
    }
    out.extend(inline_pairs(body, keys_tool.is_some()));

    // A flag on its own (`-l`, `--show`) documents an option of some tool; it is not a
    // command, and `recall cmd` printing `-L` helps nobody. (`-` alone is a vim key.)
    out.retain(|p| !is_option_only(p) && !is_not_a_command(p));

    let mut seen = std::collections::HashSet::new();
    out.retain(|p| seen.insert((p.desc.to_lowercase(), p.command.clone())));
    out.truncate(MAX_PAIRS);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v() -> Vocab {
        Vocab::seed()
    }
    fn fenced(lang: &str, body: &str) -> String {
        format!("```{}\n{}\n```", lang, body)
    }
    fn pairs(lang: &str, body: &str) -> Vec<(String, String)> {
        extract(&fenced(lang, body), &v(), None).into_iter().map(|p| (p.desc, p.command)).collect()
    }
    fn p(d: &str, c: &str) -> (String, String) {
        (d.to_string(), c.to_string())
    }

    // ── shell blocks ──

    #[test]
    fn a_comment_above_a_command_describes_it() {
        assert_eq!(
            pairs("bash", "# Update package list\nsudo apt update\n# Upgrade all packages\nsudo apt upgrade"),
            vec![p("Update package list", "sudo apt update"), p("Upgrade all packages", "sudo apt upgrade")]
        );
    }

    #[test]
    fn a_trailing_comment_describes_its_own_line() {
        assert_eq!(
            pairs("bash", "ls -la   # List files with details\ncd /tmp  # Change directory"),
            vec![p("List files with details", "ls -la"), p("Change directory", "cd /tmp")]
        );
    }

    #[test]
    fn a_comment_above_several_lines_covers_the_group_until_a_blank_line() {
        let got = pairs("bash", "# Start a web server\ncd /srv\npython3 -m http.server 8000\n\nls");
        assert_eq!(got, vec![p("Start a web server", "cd /srv\npython3 -m http.server 8000")]);
    }

    #[test]
    fn a_section_header_comment_above_annotated_lines_is_not_their_description() {
        let got = pairs("bash", "# File operations\nls -la  # List files\nmkdir -p a/b  # Create nested directories");
        assert_eq!(got, vec![p("List files", "ls -la"), p("Create nested directories", "mkdir -p a/b")]);
    }

    #[test]
    fn a_one_word_label_a_rule_or_a_shebang_is_not_a_description() {
        assert!(pairs("bash", "# Sessions\ntmux ls").is_empty());
        assert!(pairs("bash", "# =====================\nls -la").is_empty());
        assert!(pairs("bash", "#!/bin/bash\nls -la").is_empty());
        assert!(pairs("bash", "# Example:\nls -la").is_empty(), "'Example' alone says nothing");
    }

    #[test]
    fn the_last_comment_before_the_command_wins() {
        let got = pairs("bash", "# Network\n# Show listening ports\nss -tulpn");
        assert_eq!(got, vec![p("Show listening ports", "ss -tulpn")]);
    }

    #[test]
    fn a_hash_inside_quotes_or_a_url_is_not_a_comment() {
        let got = pairs("bash", "echo \"a # b\"  # Print with a hash\ncurl http://x.io/#frag  # Fetch a page");
        assert_eq!(got, vec![p("Print with a hash", "echo \"a # b\""), p("Fetch a page", "curl http://x.io/#frag")]);
    }

    #[test]
    fn a_long_group_is_a_script_not_one_command() {
        let body = format!("# Do the whole setup process\n{}", "echo step\n".repeat(9));
        assert!(pairs("bash", &body).is_empty());
    }

    #[test]
    fn output_and_annotation_words_are_not_titles() {
        assert!(pairs("bash", "ls -la  # Output: total 24\nls  # default").is_empty());
    }

    #[test]
    fn descriptions_are_cleaned_and_capitalised() {
        assert_eq!(clean_desc("find **large** files (over `100M`):"), "Find large files (over 100M)");
        assert_eq!(clean_desc("  lots   of\tspace. "), "Lots of space");
        assert!(clean_desc(&"word ".repeat(40)).chars().count() <= 90);
    }

    // ── which blocks count ──

    #[test]
    fn classify_treats_plumbing_and_config_the_way_a_person_would() {
        let vc = v();
        assert_eq!(classify("bash", "cd ~/projects\n", &vc, None), BlockKind::Shell, "cd is a command you type");
        assert_eq!(classify("bash", "source ~/.bashrc", &vc, None), BlockKind::Shell);
        assert_eq!(classify("bash", "Host myserver\n    HostName 1.2.3.4\n    Port 22", &vc, None), BlockKind::Other, "ssh config");
        assert_eq!(classify("bash", "# only a comment", &vc, None), BlockKind::Other);
        assert_eq!(classify("python", "import os", &vc, None), BlockKind::Other);
        assert_eq!(classify("powershell", "Get-Service | Where Status -eq Running", &vc, None), BlockKind::Shell);
        assert_eq!(classify("vim", "dd", &vc, None), BlockKind::Keys);
        assert_eq!(classify("tmux", "set -g mouse on", &vc, None), BlockKind::Shell);
        assert_eq!(classify("", "Use SSH when:\n- you need tunneling", &vc, None), BlockKind::Other, "a text box");
        assert_eq!(classify("", "nmap -sV 10.0.0.1", &vc, None), BlockKind::Shell);
    }

    // ── key tables ──

    #[test]
    fn vim_blocks_read_quote_comments_and_two_column_lines() {
        let body = "H            \" Toggle hidden files\n<Space>e  \" Toggle file explorer\n\"_dd\nx    delete char";
        let got = extract(&fenced("vim", body), &v(), None);
        let got: Vec<(String, String, PairKind)> = got.into_iter().map(|p| (p.desc, p.command, p.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("Toggle hidden files".to_string(), "H".to_string(), PairKind::Keys),
                ("Toggle file explorer".to_string(), "<Space>e".to_string(), PairKind::Keys),
                ("Delete char".to_string(), "x".to_string(), PairKind::Keys),
            ],
            "\"_dd is a register command, not a comment"
        );
    }

    #[test]
    fn a_bare_two_column_fence_is_a_key_table_only_in_a_vim_or_tmux_section() {
        let body = "x          delete char\ndd         delete line\nciw        change inner word";
        assert!(extract(&fenced("", body), &v(), None).is_empty(), "no keys context: just text");
        let got = extract(&fenced("", body), &v(), Some("vim"));
        assert_eq!(got.len(), 3);
        assert_eq!((got[1].desc.as_str(), got[1].command.as_str()), ("Delete line", "dd"));
    }

    #[test]
    fn a_header_row_is_not_a_key() {
        let body = "Tool      Purpose\nx         delete char\ndd        delete line";
        let got = extract(&fenced("", body), &v(), Some("vim"));
        assert_eq!(got.len(), 2, "{:?}", got);
    }

    #[test]
    fn a_box_drawn_cheat_sheet_yields_its_rows() {
        let body = "┏━━━━━━━━━━━━━━━━━━━━━━━┓\n┃ TMUX SESSIONS         ┃\n┃  tmux new -s name   Create session  ┃\n┃  Ctrl-b d           Detach          ┃\n┃  Ctrl-b %           Split vertical  ┃\n┗━━━━━━━━━━━━━━━━━━━━━━━┛";
        let got: Vec<(String, String)> =
            extract(&fenced("", body), &v(), Some("tmux")).into_iter().map(|p| (p.desc, p.command)).collect();
        assert_eq!(got, vec![p("Create session", "tmux new -s name"), p("Detach", "Ctrl-b d"), p("Split vertical", "Ctrl-b %")]);
    }

    // ── bullets and tables ──

    #[test]
    fn inline_code_bullets_are_pairs() {
        let body = "- `:set hlsearch` - Highlight search results\n* `:noh`: Clear highlighting\n1. `p` → Paste after cursor\n- `dd` delete line";
        let got: Vec<(String, String)> = inline_pairs(body, false).into_iter().map(|p| (p.desc, p.command)).collect();
        assert_eq!(got, vec![p("Highlight search results", ":set hlsearch"), p("Clear highlighting", ":noh"), p("Paste after cursor", "p")]);
    }

    #[test]
    fn prose_bullets_and_bold_names_are_not_pairs() {
        assert!(inline_pairs("- **tmux-resurrect**: Save/restore sessions\n- `sudo` is required for this\n- Use `ls` often", false).is_empty());
    }

    #[test]
    fn table_rows_are_pairs_in_either_column_order() {
        let body = "| Key | Meaning |\n|---|---|\n| `dd` | Delete line |\n| System update | `sudo apt update` | Same | [Packages](#p) |";
        let got: Vec<(String, String)> = inline_pairs(body, false).into_iter().map(|p| (p.desc, p.command)).collect();
        assert_eq!(got, vec![p("Delete line", "dd"), p("System update", "sudo apt update")]);
    }

    #[test]
    fn a_one_word_description_is_enough_for_a_key_but_not_for_a_shell_command() {
        assert!(pairs("bash", "kill -15 PID  # Graceful").is_empty());
        assert!(inline_pairs("| `\\` | Backslash |", false).is_empty());
        assert_eq!(inline_pairs("- `p` - Paste", true).len(), 1, "keys section");
        let got = extract(&fenced("vim", "p    \" Paste"), &v(), None);
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn config_directives_and_notation_are_not_commands_but_assignments_and_cmdlets_are() {
        // ssh config in a bash fence
        assert!(pairs("bash", "# Old method with a jump host\nHost target\n    ProxyCommand ssh jump -W %h:%p").is_empty());
        assert!(pairs("bash", "Banner /etc/ssh/banner.txt  # Show a banner before login").is_empty());
        // ...but an assignment and a PowerShell cmdlet are typed at a prompt
        assert_eq!(pairs("bash", "LD_PRELOAD=/tmp/x.so ls  # Preload a library for one run").len(), 1);
        assert_eq!(pairs("powershell", "Get-Process  # List running processes now").len(), 1);
        // regex / glob / path notation in a table
        let body = "| `[a-zA-Z]{2,}` | Two or more letters |\n| `*.log` | Any log file |\n| `/var/` | Variable data |\n| `/etc/passwd` | System users |\n| `2>&1` | Redirect errors too |\n| `conv=` | Conversion options |\n| `./run.sh` | Run the script here |\n| `/opt/tools/scan.sh` | Run the scanner script |\n| `!!` | Repeat the last command |";
        let got: Vec<String> = extract(body, &v(), None).into_iter().map(|p| p.command).collect();
        assert_eq!(got, vec!["./run.sh", "/opt/tools/scan.sh", "!!"]);
    }

    #[test]
    fn a_flag_on_its_own_is_not_a_command() {
        let body = "- `-l` - Count lines only\n- `--show` - Show cracked passwords\n| `-L` | Local port forwarding |\n- `wc -l` - Count lines";
        let got: Vec<(String, String)> = extract(body, &v(), None).into_iter().map(|p| (p.desc, p.command)).collect();
        assert_eq!(got, vec![p("Count lines", "wc -l")]);
        let keys = extract(&fenced("vim", "-    \" Navigate up one directory"), &v(), None);
        assert_eq!(keys.len(), 1, "a lone `-` is a vim key");
    }

    #[test]
    fn code_inside_fences_is_left_to_the_fence_parser() {
        assert!(inline_pairs("```\n- `ls` - list files\n```", false).is_empty());
    }

    // ── the whole section ──

    #[test]
    fn duplicates_within_a_section_collapse_and_the_count_is_capped() {
        let body = fenced("bash", "ls -la  # List files\nls -la  # List files");
        assert_eq!(extract(&body, &v(), None).len(), 1);
        let many: String = (0..200).map(|i| format!("cmd{} -x  # Does thing number {}\n", i, i)).collect();
        assert_eq!(extract(&fenced("bash", &many), &v(), None).len(), MAX_PAIRS);
    }
}
