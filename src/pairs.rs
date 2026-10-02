//! Where the commands in a section are.
//!
//! Commands live in fenced blocks (shell, or a vim/tmux key table) and in
//! cheat-sheet lines outside them: a `` - `cmd` - description `` bullet or a
//! table row. This module decides which fenced blocks *are* command blocks and
//! finds those bullet/table pairs, so the importer can weigh how much of a
//! section is commands and how much is prose.

use crate::derive::{self, Vocab};

#[derive(Debug, Clone, PartialEq)]
pub struct Pair {
    /// What it does.
    pub desc:    String,
    /// What to type.
    pub command: String,
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

/// Is `d` a description: real words, not a section marker or an annotation?
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

/// A description next to a command. Two words in a shell or bullet context — a lone word
/// (`Graceful`) is an annotation; one is enough for a key (`Ctrl-b d` — Detach).
fn trailing_desc(c: &str, min_words: usize) -> Option<String> {
    let d = clean_desc(c);
    valid_desc(&d, min_words).then_some(d)
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

/// Is this line a bullet or table row that pairs inline code with a description?
pub fn is_pair_line(l: &str) -> bool {
    bullet_pair(l).or_else(|| table_pair(l)).is_some()
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
                out.push(Pair { desc: d, command: code });
            }
        }
    }
    out
}

/// Is the code of a bullet/table pair something you would type at a prompt?
///
/// A flag on its own (`-l`, `--show`) documents an option of some tool, and cheat
/// sheets for regex, globs and paths list *notation* (`[a-zA-Z]{2,}`, `*.log`, `/var/`,
/// `2>&1`, `$0`) — describable, but not commands. A command starts like one, or is a
/// path to a program.
pub fn is_typed_command(c: &str) -> bool {
    let c = c.trim();
    if c.starts_with('-') || c.ends_with('=') {
        return false; // an option (`-L`), or a dangling one (`conv=`)
    }
    let first = c.split_whitespace().next().unwrap_or("");
    // A lone absolute path is a "what lives where" table row (`/etc/passwd` — System users),
    // not something to run — unless it is a script.
    let lone = c.split_whitespace().count() == 1;
    let script = [".sh", ".bash", ".py", ".pl", ".rb", ".php"].iter().any(|e| first.ends_with(e));
    if lone && first.starts_with('/') && !script {
        return false;
    }
    let path = (first.starts_with("./") || first.starts_with("~/") || first.starts_with('/'))
        && !first.ends_with('/')
        && first.len() > 2;
    first.starts_with(|ch: char| ch.is_ascii_alphabetic())
        || path
        || (first.starts_with('!') && first.len() >= 2) // history expansion: !! !$
        || (first.starts_with(':') && first.len() >= 2) // ex command
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v() -> Vocab {
        Vocab::seed()
    }
    fn p(d: &str, c: &str) -> (String, String) {
        (d.to_string(), c.to_string())
    }
    fn pairs(body: &str) -> Vec<(String, String)> {
        inline_pairs(body, false).into_iter().map(|p| (p.desc, p.command)).collect()
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
        assert_eq!(classify("bash", "LD_PRELOAD=/tmp/x.so ls", &vc, None), BlockKind::Shell, "an assignment is typed");
        assert_eq!(classify("vim", "dd", &vc, None), BlockKind::Keys);
        assert_eq!(classify("tmux", "set -g mouse on", &vc, None), BlockKind::Shell);
        assert_eq!(classify("", "Use SSH when:\n- you need tunneling", &vc, None), BlockKind::Other, "a text box");
        assert_eq!(classify("", "nmap -sV 10.0.0.1", &vc, None), BlockKind::Shell);
    }

    #[test]
    fn a_bare_two_column_fence_is_a_key_table_only_in_a_vim_or_tmux_section() {
        let body = "x          delete char\ndd         delete line\nciw        change inner word";
        assert_eq!(classify("", body, &v(), Some("vim")), BlockKind::Keys);
        assert_ne!(classify("", body, &v(), None), BlockKind::Keys, "no keys context: not a key table");
        let boxed = "┏━━━━━━━━━━━━━━━━━━━━━━━┓\n┃  tmux new -s name   Create session  ┃\n┃  Ctrl-b d           Detach          ┃\n┗━━━━━━━━━━━━━━━━━━━━━━━┛";
        assert_eq!(classify("", boxed, &v(), Some("tmux")), BlockKind::Keys, "a box-drawn cheat sheet is a key table");
    }

    // ── bullets and tables ──

    #[test]
    fn inline_code_bullets_are_pairs() {
        let body = "- `:set hlsearch` - Highlight search results\n* `:noh`: Clear highlighting\n1. `p` → Paste after cursor\n- `dd` delete line";
        assert_eq!(pairs(body), vec![p("Highlight search results", ":set hlsearch"), p("Clear highlighting", ":noh"), p("Paste after cursor", "p")]);
        assert!(is_pair_line("- `ls -la` - List everything") && !is_pair_line("- Use `ls` often"));
    }

    #[test]
    fn prose_bullets_and_bold_names_are_not_pairs() {
        assert!(pairs("- **tmux-resurrect**: Save/restore sessions\n- `sudo` is required for this\n- Use `ls` often").is_empty());
    }

    #[test]
    fn table_rows_are_pairs_in_either_column_order() {
        let body = "| Key | Meaning |\n|---|---|\n| `dd` | Delete line |\n| System update | `sudo apt update` | Same | [Packages](#p) |";
        assert_eq!(pairs(body), vec![p("Delete line", "dd"), p("System update", "sudo apt update")]);
    }

    #[test]
    fn a_one_word_description_is_enough_for_a_key_but_not_for_a_shell_command() {
        assert!(pairs("| `\\` | Backslash |").is_empty());
        assert_eq!(inline_pairs("- `p` - Paste", true).len(), 1, "keys section");
    }

    #[test]
    fn output_and_annotation_words_are_not_descriptions() {
        assert!(pairs("- `ls -la` - Output\n- `ls` - default").is_empty());
    }

    #[test]
    fn descriptions_are_cleaned_and_capitalised() {
        assert_eq!(clean_desc("find **large** files (over `100M`):"), "Find large files (over 100M)");
        assert_eq!(clean_desc("  lots   of\tspace. "), "Lots of space");
        assert!(clean_desc(&"word ".repeat(40)).chars().count() <= 90);
    }

    #[test]
    fn code_inside_fences_is_left_to_the_fence_parser() {
        assert!(pairs("```\n- `ls` - list files\n```").is_empty());
    }

    #[test]
    fn options_and_notation_are_not_typed_commands_but_programs_and_scripts_are() {
        for c in ["-l", "--show", "conv=", "[a-zA-Z]{2,}", "*.log", "/var/", "/etc/passwd", "2>&1", "$0"] {
            assert!(!is_typed_command(c), "{:?} is notation, not a command", c);
        }
        for c in ["wc -l", "sudo apt update", "./run.sh", "/opt/tools/scan.sh", "!!", ":noh"] {
            assert!(is_typed_command(c), "{:?} is typed at a prompt", c);
        }
    }
}
