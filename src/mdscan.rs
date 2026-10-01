//! Structural scan of messy notes: what is code, what is a heading, what is
//! just page furniture.
//!
//! This is the part of the importer that decides *where one entry ends and the
//! next begins*. Every rule exists because a real notes file broke the simple
//! approach — a bad rule here glues unrelated text into one entry (or cuts one
//! in two), so each pass is a pure function of the text and is unit-tested:
//!
//!   1. Page furniture. A web page or chat printed to PDF repeats a header
//!      ("3/15/26, 11:40 PM" + title) and footer (URL + "6/103") on every
//!      page. Three or more identical ones are removed.
//!   2. Fences. Code blocks are paired the way a person reads them: a
//!      ```markdown wrapper is unwrapped, a stray fence is dropped and an
//!      unclosed block is closed where it visibly ends — so one bad fence can't
//!      flip "code / not code" for the rest of the file.
//!   3. Headings. `#` lines inside code are comments. Outside code they are
//!      headings unless they sit in a run of commands or comments. Plain-text
//!      headings ("3️⃣ Strong passphrase", "Step 14 — Scan files") count once
//!      they clearly head a body, and a "heading" that is really a list item
//!      ("Good systems use:" / "## PBKDF2") is turned back into one.

use std::collections::HashSet;

use crate::derive::Vocab;

/// One line of the cleaned document.
#[derive(Debug, Clone)]
pub struct Line {
    pub text: String,
    /// Inside a fenced code block (the fence lines themselves included).
    pub code: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Cleanup {
    /// Page header/footer lines removed.
    pub furniture: usize,
    /// Fence lines with no partner, dropped.
    pub stray_fences: usize,
    /// Code blocks that never closed, closed where they visibly ended.
    pub closed_fences: usize,
    /// ```markdown wrappers removed (what is inside is ordinary Markdown).
    pub unwrapped: usize,
}

/// A stretch of the document that came from one printed page series, e.g. the
/// 103-page "Unbreakable Encryption Methods" chat export.
#[derive(Debug, Clone)]
pub struct PrintedDoc {
    pub title: String,
    /// Line range in the cleaned document, inclusive.
    pub from: usize,
    pub to: usize,
}

pub struct Normalized {
    pub lines: Vec<Line>,
    pub cleanup: Cleanup,
    pub printed: Vec<PrintedDoc>,
}

// ─── pass 1: page furniture ──────────────────────────────────────────────────

/// "3/15/26, 11:40 PM", "15/03/2026, 23:40:12" — the date/time a browser
/// prints at the top of every page.
fn is_stamp(l: &str) -> bool {
    let Some((date, time)) = l.trim().split_once(", ") else { return false };
    let digits = |p: &str, max: usize| !p.is_empty() && p.len() <= max && p.chars().all(|c| c.is_ascii_digit());
    let dp: Vec<&str> = date.split(['/', '-', '.']).collect();
    if dp.len() != 3 || !dp.iter().all(|p| digits(p, 4)) {
        return false;
    }
    let time = time.trim();
    let (clock, ampm) = match time.split_once(' ') {
        Some((c, a)) => (c, Some(a)),
        None => (time, None),
    };
    let cp: Vec<&str> = clock.split(':').collect();
    (2..=3).contains(&cp.len())
        && cp.iter().all(|p| digits(p, 2))
        && ampm.map_or(true, |a| matches!(a.to_ascii_uppercase().as_str(), "AM" | "PM"))
}

/// "6/103" — page 6 of 103.
fn page_counter(l: &str) -> Option<u32> {
    let (a, b) = l.trim().split_once('/')?;
    let num = |s: &str| (!s.is_empty() && s.len() <= 4 && s.chars().all(|c| c.is_ascii_digit())).then(|| s.parse::<u32>().ok()).flatten();
    let (a, b) = (num(a)?, num(b)?);
    (a >= 1 && a <= b && b >= 3).then_some(b)
}

fn is_url_line(l: &str) -> bool {
    let t = l.trim().trim_start_matches('<').trim_end_matches('>');
    (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace)
}

/// Lines a chat page prints above its own header; only removed when they sit
/// right next to page furniture.
fn is_page_chrome(l: &str) -> bool {
    matches!(l.trim(), "ChatGPT" | "Get Plus")
}

fn prev_nonblank(src: &[&str], from: usize) -> Option<usize> {
    (0..from).rev().find(|&j| !src[j].trim().is_empty())
}

/// Returns which lines to keep, plus the printed documents found (raw ranges).
fn strip_furniture(src: &[&str], cleanup: &mut Cleanup) -> (Vec<bool>, Vec<(String, usize, usize)>) {
    struct Block {
        start: usize,
        end: usize,
        total: u32,
        title: String,
    }
    let mut blocks: Vec<Block> = Vec::new();
    for k in 0..src.len() {
        let Some(total) = page_counter(src[k]) else { continue };
        let Some(u) = prev_nonblank(src, k) else { continue };
        if !is_url_line(src[u]) {
            continue;
        }
        let Some(s) = (u.saturating_sub(6)..u).rev().find(|&j| is_stamp(src[j])) else { continue };
        let title = src[s + 1..u].iter().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("").to_string();
        blocks.push(Block { start: s, end: k, total, title });
    }

    let mut keep = vec![true; src.len()];
    let mut docs: Vec<(String, usize, usize)> = Vec::new();
    let mut totals: Vec<u32> = blocks.iter().map(|b| b.total).collect();
    totals.sort_unstable();
    totals.dedup();
    for total in totals {
        let group: Vec<&Block> = blocks.iter().filter(|b| b.total == total).collect();
        if group.len() < 3 {
            continue; // one date + URL + fraction is a coincidence; a series is furniture
        }
        for b in &group {
            let mut start = b.start;
            // page chrome directly above the header (allowing blank lines between)
            for _ in 0..3 {
                match prev_nonblank(src, start) {
                    Some(p) if is_page_chrome(src[p]) && keep[p] => start = p,
                    _ => break,
                }
            }
            for flag in &mut keep[start..=b.end] {
                *flag = false;
            }
        }
        docs.push((group[0].title.clone(), group[0].start, group[group.len() - 1].end));
    }
    cleanup.furniture = keep.iter().zip(src).filter(|(k, l)| !**k && !l.trim().is_empty()).count();
    (keep, docs)
}

// ─── pass 2: fences ──────────────────────────────────────────────────────────

struct Tok {
    at: usize,
    ch: char,
    len: usize,
    lang: String,
    info: String,
}

impl Tok {
    fn has_info(&self) -> bool {
        !self.info.is_empty()
    }
}

fn fence_tok(line: &str, at: usize) -> Option<Tok> {
    let t = line.trim_start();
    let ch = t.chars().next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let len = t.chars().take_while(|&c| c == ch).count();
    if len < 3 {
        return None;
    }
    let info = t[len..].trim();
    if ch == '`' && info.contains('`') {
        return None; // ```inline code``` on one line
    }
    let lang = info.split(|c: char| c.is_whitespace() || c == '{').next().unwrap_or("").to_lowercase();
    Some(Tok { at, ch, len, lang, info: info.to_string() })
}

fn closes(closer: &Tok, opener: &Tok) -> bool {
    !closer.has_info() && closer.ch == opener.ch && closer.len >= opener.len
}

struct Block {
    open: usize,
    close: Option<usize>,
    /// Line of the next fence token: an unclosed block cannot run past it.
    limit: usize,
}

#[derive(Default)]
struct Pairing {
    blocks: Vec<Block>,
    noise: Vec<usize>,
    unwrapped: usize,
}

fn is_markdown_lang(l: &str) -> bool {
    matches!(l, "markdown" | "md")
}

/// Pair fence tokens the way a person reads them.
///
/// * A fence with an info string (```bash) can only open a block.
/// * A bare fence opens a block only if the next fence can close it; if the
///   next one carries an info string, the bare fence was a stray closer and is
///   dropped rather than allowed to turn the following prose into "code".
/// * A ```markdown fence wraps a whole document: fences nested inside it are
///   paired among themselves and the wrapper's own two lines are dropped.
fn pair(toks: &[Tok], all_at: &[usize], end: usize, p: &mut Pairing) {
    let next_limit = |t: &Tok| all_at.get(all_at.partition_point(|&a| a <= t.at)).copied().unwrap_or(end);
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if !t.has_info() {
            if toks.get(i + 1).is_some_and(|n| closes(n, t)) {
                p.blocks.push(Block { open: t.at, close: Some(toks[i + 1].at), limit: 0 });
                i += 2;
            } else {
                p.noise.push(t.at);
                i += 1;
            }
            continue;
        }
        if is_markdown_lang(&t.lang) {
            let mut depth = 0usize;
            let mut closer = None;
            for (j, n) in toks.iter().enumerate().skip(i + 1) {
                if n.has_info() {
                    depth += 1;
                } else if depth > 0 {
                    depth -= 1;
                } else {
                    closer = Some(j);
                    break;
                }
            }
            p.noise.push(t.at);
            match closer {
                Some(c) => {
                    p.noise.push(toks[c].at);
                    p.unwrapped += 1;
                    pair(&toks[i + 1..c], all_at, end, p);
                    i = c + 1;
                }
                None => i += 1, // unclosed wrapper: drop just its opener
            }
            continue;
        }
        if toks.get(i + 1).is_some_and(|n| closes(n, t)) {
            p.blocks.push(Block { open: t.at, close: Some(toks[i + 1].at), limit: 0 });
            i += 2;
        } else {
            p.blocks.push(Block { open: t.at, close: None, limit: next_limit(t) });
            i += 1;
        }
    }
}

/// Where an unclosed block visibly ends: before the next fence, or before the
/// next `##`–`####` heading that follows a blank line, whichever is first —
/// minus trailing blank lines.
fn unclosed_end(src: &[&str], open: usize, limit: usize) -> usize {
    let mut end_excl = limit;
    for k in open + 1..limit {
        if src[k - 1].trim().is_empty() && atx(src[k]).is_some_and(|(lvl, _)| (2..=4).contains(&lvl)) {
            end_excl = k;
            break;
        }
    }
    let mut last = end_excl.saturating_sub(1).max(open);
    while last > open && src[last].trim().is_empty() {
        last -= 1;
    }
    last
}

/// Returns the cleaned lines and, for each, the index of the source line it
/// came from (non-decreasing — used to map ranges through the cleanup).
fn fix_fences(src: &[&str], cleanup: &mut Cleanup) -> (Vec<Line>, Vec<usize>) {
    let toks: Vec<Tok> = src.iter().enumerate().filter_map(|(i, l)| fence_tok(l, i)).collect();
    let all_at: Vec<usize> = toks.iter().map(|t| t.at).collect();
    let mut p = Pairing::default();
    pair(&toks, &all_at, src.len(), &mut p);
    cleanup.unwrapped = p.unwrapped;

    #[derive(Clone, Copy, PartialEq)]
    enum Role {
        Text,
        Code,
        Drop,
    }
    let mut role = vec![Role::Text; src.len()];
    let mut close_after: HashSet<usize> = HashSet::new();
    for &d in &p.noise {
        role[d] = Role::Drop;
    }
    cleanup.stray_fences = p.noise.len().saturating_sub(2 * p.unwrapped);
    for b in &p.blocks {
        match b.close {
            Some(c) => role[b.open..=c].iter_mut().for_each(|r| *r = Role::Code),
            None => {
                let end = unclosed_end(src, b.open, b.limit);
                if end <= b.open {
                    role[b.open] = Role::Drop; // an opener with nothing after it
                    cleanup.stray_fences += 1;
                    continue;
                }
                role[b.open..=end].iter_mut().for_each(|r| *r = Role::Code);
                close_after.insert(end);
                cleanup.closed_fences += 1;
            }
        }
    }

    let mut lines = Vec::with_capacity(src.len() + close_after.len());
    let mut origin = Vec::with_capacity(src.len() + close_after.len());
    for (i, l) in src.iter().enumerate() {
        match role[i] {
            Role::Drop => {}
            Role::Text => {
                lines.push(Line { text: (*l).to_string(), code: false });
                origin.push(i);
            }
            Role::Code => {
                // tilde fences and >3-backtick fences become plain ``` so the
                // rest of the program (which only knows ```) reads them
                let text = match fence_tok(l, i) {
                    Some(t) if t.ch != '`' || t.len != 3 => {
                        let indent = &l[..l.len() - l.trim_start().len()];
                        format!("{}```{}", indent, t.info)
                    }
                    _ => (*l).to_string(),
                };
                lines.push(Line { text, code: true });
                origin.push(i);
            }
        }
        if close_after.contains(&i) {
            lines.push(Line { text: "```".to_string(), code: true });
            origin.push(i);
        }
    }
    (lines, origin)
}

/// Clean raw text into labelled lines (passes 1 and 2).
pub fn normalize(text: &str) -> Normalized {
    let raw: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let mut cleanup = Cleanup::default();
    let (keep, docs) = strip_furniture(&raw, &mut cleanup);
    let kept: Vec<usize> = (0..raw.len()).filter(|&i| keep[i]).collect();
    let kept_lines: Vec<&str> = kept.iter().map(|&i| raw[i]).collect();
    let (lines, origin) = fix_fences(&kept_lines, &mut cleanup);

    let to_out = |raw_i: usize| {
        let k = kept.partition_point(|&r| r < raw_i);
        origin.partition_point(|&o| o < k).min(lines.len().saturating_sub(1))
    };
    let printed = docs
        .into_iter()
        .map(|(title, from, to)| PrintedDoc { title, from: to_out(from), to: to_out(to) })
        .collect();
    Normalized { lines, cleanup, printed }
}

// ─── pass 3: headings ────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadKind {
    /// `#` … `######`
    Atx,
    /// `# ====` / `# TITLE` / `# ====`
    Banner,
    /// `3️⃣ Title`
    Keycap,
    /// `Step 14 — Title`
    Step,
}

impl HeadKind {
    pub fn is_soft(self) -> bool {
        matches!(self, HeadKind::Keycap | HeadKind::Step)
    }
}

#[derive(Debug, Clone)]
pub struct Heading {
    pub at: usize,
    pub level: u8,
    pub raw: String,
    pub kind: HeadKind,
}

pub struct HeadingScan {
    pub heads: Vec<Heading>,
    /// Lines that were `##` headings but really list items, rewritten as `- item`.
    pub demoted: Vec<usize>,
}

/// `## Title` → (2, "Title"). The closing `#`s of `## Title ##` are dropped
/// only when preceded by a space, so `# Learn C#` keeps its `#`.
pub fn atx(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|&b| b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let body = line[hashes..].strip_prefix(' ').or_else(|| line[hashes..].strip_prefix('\t'))?.trim();
    let stripped = body.trim_end_matches('#');
    let title = if stripped.len() < body.len() && stripped.ends_with(' ') {
        stripped.trim_end()
    } else if stripped.is_empty() {
        return None;
    } else {
        body
    };
    (!title.is_empty()).then_some((hashes as u8, title))
}

/// `====`, `----`, `# ********` — decoration, not a title.
fn rule_only(s: &str) -> bool {
    s.len() >= 3 && s.chars().all(|c| matches!(c, '=' | '-' | '_' | '*' | '#' | '~' | ' '))
}

fn is_rule_line(l: &Line) -> bool {
    !l.code && atx(&l.text).is_some_and(|(lvl, t)| lvl == 1 && rule_only(t))
}

fn blank_at(lines: &[Line], i: isize) -> bool {
    i < 0 || i as usize >= lines.len() || lines[i as usize].text.trim().is_empty()
}

/// Shell syntax that isn't a program invocation: `VPN_IF="wg0"`, `set -e`,
/// `fi`, `start_vpn()`, a bare `snake_case_call`.
fn shell_syntax(t: &str) -> bool {
    let t = t.trim();
    if let Some((key, _)) = t.split_once('=') {
        if key.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            return true;
        }
    }
    let first = t.split_whitespace().next().unwrap_or("");
    matches!(
        first,
        "set" | "export" | "source" | "." | "if" | "then" | "else" | "elif" | "fi" | "for" | "while" | "until"
            | "do" | "done" | "case" | "esac" | "function" | "local" | "return" | "exit" | "}" | "{" | "[[" | "trap"
            | "shift" | "unset"
    ) || t.ends_with("() {")
        || t.ends_with("(){")
        || t.starts_with("#!")
        || (t.contains('_') && t.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
}

/// Does this line look like part of a script — a command, shell syntax or a `#` comment?
/// Fence lines don't count: a heading right after a closing fence is a heading.
fn scripty(l: &Line, vocab: &Vocab) -> bool {
    if l.code {
        return false;
    }
    let t = l.text.trim_start();
    let hash_comment = t.starts_with('#') && !atx(t).is_some_and(|(lvl, _)| lvl >= 2);
    hash_comment
        || l.text.starts_with("    ")
        || l.text.starts_with('\t')
        || vocab.command_line(t).is_some()
        || shell_syntax(t)
}

/// A `# text` line outside code is a comment, not a heading, when it sits in a
/// run of commands/comments: `# 3️⃣ Encrypt the vault` right above `sudo cryptsetup …`.
fn h1_is_comment(lines: &[Line], i: usize, vocab: &Vocab) -> bool {
    (i > 0 && scripty(&lines[i - 1], vocab)) || lines.get(i + 1).is_some_and(|l| scripty(l, vocab))
}

/// `# ====` / `# TITLE` / `# ====`
fn is_banner_title(lines: &[Line], i: usize) -> bool {
    i > 0 && i + 1 < lines.len() && is_rule_line(&lines[i - 1]) && is_rule_line(&lines[i + 1])
}

/// A converter that flattens a list or table can turn its items into headings:
///
/// ```text
/// Good systems use:          Option
/// ## PBKDF2                  Meaning
/// Argon2                     ## AES-256-GCM
///                            authenticated encryption
///                            ## PBKDF2
///                            protects password against brute force
/// ```
///
/// Two shapes qualify, both requiring a *short* heading (three words or fewer):
/// it follows a line ending in ':' and is followed by a short plain line; or
/// it sits in a run of short heading / short line / heading / short line
/// pairs. A lone heading with a one-line description does not qualify — the
/// run needs two consecutive short descriptions — so real headings survive.
fn is_list_item_heading(lines: &[Line], i: usize, level: u8, title: &str) -> bool {
    if title.split_whitespace().count() > 3 {
        return false;
    }
    let plain = |j: usize, max_words: usize| {
        let t = lines[j].text.trim();
        !lines[j].code
            && !t.is_empty()
            && t.split_whitespace().count() <= max_words
            && !t.ends_with(['.', '!', '?', ':'])
            && !t.starts_with(['#', '`', '-', '*', '+', '|', '>'])
    };
    let short_heading = |j: usize| {
        !lines[j].code && atx(&lines[j].text).is_some_and(|(l, t)| l == level && t.split_whitespace().count() <= 3)
    };
    let prev: Vec<usize> = (0..i).rev().filter(|&j| !lines[j].text.trim().is_empty()).take(2).collect();
    let next: Vec<usize> = (i + 1..lines.len()).filter(|&j| !lines[j].text.trim().is_empty()).take(3).collect();

    let lead_in = prev.first().is_some_and(|&p| {
        let t = lines[p].text.trim_end();
        !lines[p].code && t.ends_with(':') && !t.starts_with('#')
    });
    if lead_in && next.first().is_some_and(|&n| plain(n, 4)) {
        return true;
    }
    let forward = next.len() == 3 && plain(next[0], 6) && short_heading(next[1]) && plain(next[2], 6);
    let backward = prev.len() == 2
        && next.first().is_some_and(|&n| plain(n, 6))
        && plain(prev[0], 6)
        && short_heading(prev[1]);
    forward || backward
}

/// "3️⃣ Strong passphrase" → "Strong passphrase".
fn keycap(t: &str) -> Option<&str> {
    let t = t.trim();
    let mut chars = t.chars();
    let first = chars.next()?;
    let rest = if first == '\u{1F51F}' {
        chars.as_str()
    } else if first.is_ascii_digit() {
        let r = chars.as_str();
        r.strip_prefix('\u{FE0F}').unwrap_or(r).strip_prefix('\u{20E3}')?
    } else {
        return None;
    };
    let title = rest.trim_start();
    (!title.is_empty() && rest.starts_with(char::is_whitespace)).then_some(title)
}

/// "Step 14 — Scan suspicious files" → the whole line (the label is part of the title).
fn step(t: &str) -> Option<&str> {
    let t = t.trim();
    let mut words = t.splitn(3, char::is_whitespace);
    let label = words.next()?.to_lowercase();
    if !matches!(label.as_str(), "step" | "phase" | "stage" | "part" | "chapter" | "lesson" | "module") {
        return None;
    }
    let num = words.next()?;
    let num_body = num.trim_end_matches([':', '.', ')', '-', '–', '—']);
    let is_num = !num_body.is_empty()
        && (num_body.chars().all(|c| c.is_ascii_digit()) || num_body.chars().all(|c| matches!(c, 'I' | 'V' | 'X' | 'i' | 'v' | 'x')));
    if !is_num {
        return None;
    }
    // a separator either sticks to the number ("Step 3:") or follows it ("Step 3 —")
    let tail = words.next().unwrap_or("").trim_start();
    let has_sep = num.len() != num_body.len()
        || tail.starts_with([':', '.', ')', '-', '–', '—']);
    (has_sep && tail.trim_start_matches([':', '.', ')', '-', '–', '—', ' ']).len() >= 2).then_some(t)
}

fn soft_candidate(lines: &[Line], i: usize) -> Option<(HeadKind, String)> {
    let t = lines[i].text.trim();
    if t.chars().count() > 110 || t.ends_with(['.', ',', ';']) {
        return None;
    }
    if !blank_at(lines, i as isize - 1) || !blank_at(lines, i as isize + 1) {
        return None;
    }
    if let Some(title) = keycap(t) {
        return Some((HeadKind::Keycap, title.to_string()));
    }
    step(t).map(|s| (HeadKind::Step, s.to_string()))
}

/// Find the headings of a normalized document (pass 3). Demoted list items are
/// rewritten in place as `- item`.
pub fn find_headings(lines: &mut [Line], vocab: &Vocab) -> HeadingScan {
    let mut heads: Vec<Heading> = Vec::new();
    let mut demote: Vec<(usize, String)> = Vec::new();

    for i in 0..lines.len() {
        if lines[i].code {
            continue;
        }
        if let Some((lvl, title)) = atx(&lines[i].text) {
            if lvl == 1 {
                if rule_only(title) {
                    continue;
                }
                if is_banner_title(lines, i) {
                    heads.push(Heading { at: i, level: 1, raw: title.to_string(), kind: HeadKind::Banner });
                } else if !h1_is_comment(lines, i, vocab) {
                    heads.push(Heading { at: i, level: 1, raw: title.to_string(), kind: HeadKind::Atx });
                }
            } else if is_list_item_heading(lines, i, lvl, title) {
                demote.push((i, format!("- {}", title)));
            } else {
                heads.push(Heading { at: i, level: lvl, raw: title.to_string(), kind: HeadKind::Atx });
            }
            continue;
        }
        if let Some((kind, title)) = soft_candidate(lines, i) {
            heads.push(Heading { at: i, level: 0, raw: title, kind });
        }
    }

    // A plain-text heading must head a body. `1️⃣ A` / `2️⃣ B` / `3️⃣ C` on
    // consecutive paragraphs with nothing under them is a list, not three sections.
    let mut keep = vec![true; heads.len()];
    for c in 0..heads.len() {
        if !heads[c].kind.is_soft() {
            continue;
        }
        let next = heads.get(c + 1).map_or(lines.len(), |h| h.at);
        if !(heads[c].at + 1..next).any(|j| !lines[j].text.trim().is_empty()) {
            keep[c] = false;
        }
    }
    let mut k = keep.iter();
    heads.retain(|_| *k.next().unwrap());

    // Soft headings nest one level under the closest real heading above them.
    let mut last_real = 0u8;
    for h in &mut heads {
        if h.kind.is_soft() {
            h.level = (last_real + 1).min(6);
        } else {
            last_real = h.level;
        }
    }

    let demoted = demote.iter().map(|(i, _)| *i).collect();
    for (i, text) in demote {
        lines[i].text = text;
    }
    HeadingScan { heads, demoted }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(t: &str) -> Normalized {
        normalize(t)
    }
    fn texts(n: &Normalized) -> Vec<&str> {
        n.lines.iter().map(|l| l.text.as_str()).collect()
    }
    fn heads(t: &str) -> Vec<(u8, String, HeadKind)> {
        let mut n = norm(t);
        find_headings(&mut n.lines, &Vocab::seed()).heads.into_iter().map(|h| (h.level, h.raw, h.kind)).collect()
    }

    // ── page furniture ──

    fn page(n: u32, title: &str) -> String {
        format!("3/15/26, 11:40 PM\n{t}\n\n<https://chatgpt.com/c/abc>\n{n}/103\n", t = title, n = n)
    }

    #[test]
    fn a_series_of_page_headers_is_removed_and_text_rejoins() {
        let t = format!("Good systems use:\n\n{}\nArgon2\n\n{}\nscrypt\n\n{}\nend\n", page(6, "Doc"), page(7, "Doc"), page(8, "Doc"));
        let n = norm(&t);
        let joined = texts(&n).join("\n");
        assert!(!joined.contains("chatgpt.com") && !joined.contains("11:40") && !joined.contains("6/103"));
        assert!(joined.contains("Good systems use:") && joined.contains("Argon2") && joined.contains("scrypt"));
        assert_eq!(n.cleanup.furniture, 12); // 4 non-blank lines x 3 pages
        assert_eq!(n.printed.len(), 1);
        assert_eq!(n.printed[0].title, "Doc");
    }

    #[test]
    fn one_date_url_and_fraction_is_not_furniture() {
        let t = format!("intro\n\n{}\nbody\n", page(1, "Doc"));
        let n = norm(&t);
        assert!(texts(&n).join("\n").contains("chatgpt.com"), "a lone match is content");
        assert_eq!(n.cleanup.furniture, 0);
    }

    #[test]
    fn chat_chrome_next_to_furniture_goes_too() {
        let t = format!("x\n\nChatGPT\nGet Plus\n\n{}\ny\n\n{}\nz\n\n{}\n", page(1, "Doc"), page(2, "Doc"), page(3, "Doc"));
        let n = norm(&t);
        let joined = texts(&n).join("\n");
        assert!(!joined.contains("ChatGPT") && !joined.contains("Get Plus"));
    }

    #[test]
    fn stamps_and_counters_are_recognised_strictly() {
        assert!(is_stamp("3/15/26, 11:40 PM") && is_stamp("15/03/2026, 23:40:12"));
        assert!(!is_stamp("Monday, 9 March") && !is_stamp("1/2, 3:4 PM extra"));
        assert_eq!(page_counter("6/103"), Some(103));
        assert_eq!(page_counter("3/4"), Some(4));
        assert_eq!(page_counter("1/2"), None, "two-page docs are not evidence");
        assert_eq!(page_counter("7/5"), None);
        assert_eq!(page_counter("and/or"), None);
    }

    // ── fences ──

    #[test]
    fn markdown_wrapper_is_unwrapped_and_inner_fences_stay_code() {
        let t = "intro\n```markdown\n## Inner\n\n```bash\nls\n```\n\nafter\n```\ntail\n";
        let n = norm(t);
        let tx = texts(&n);
        assert!(!tx.contains(&"```markdown"));
        assert_eq!(n.cleanup.unwrapped, 1);
        let inner = n.lines.iter().position(|l| l.text == "## Inner").unwrap();
        assert!(!n.lines[inner].code, "wrapper content is ordinary markdown");
        let ls = n.lines.iter().position(|l| l.text == "ls").unwrap();
        assert!(n.lines[ls].code);
    }

    #[test]
    fn a_stray_bare_fence_does_not_flip_the_rest_of_the_file() {
        // The lone ``` is followed by a fence with a language, so it can only be a stray.
        let t = "prose\n```\nmore prose\n## Real heading\n\n```bash\nls\n```\n";
        let n = norm(t);
        assert_eq!(n.cleanup.stray_fences, 1);
        let h = n.lines.iter().position(|l| l.text == "## Real heading").unwrap();
        assert!(!n.lines[h].code);
    }

    #[test]
    fn an_unclosed_block_is_closed_where_it_visibly_ends() {
        let t = "```bash\nid\nuname -a\n\n## Next section\n\ntext\n```bash\nls\n```\n";
        let n = norm(t);
        assert_eq!(n.cleanup.closed_fences, 1);
        let tx = texts(&n);
        let close = tx.iter().position(|l| *l == "```").unwrap();
        let next = tx.iter().position(|l| *l == "## Next section").unwrap();
        assert!(close < next, "block closed before the heading: {:?}", tx);
        assert!(!n.lines[next].code);
    }

    #[test]
    fn tilde_and_long_fences_become_plain_backticks() {
        let n = norm("~~~bash\nls\n~~~\n````\nx\n````\n");
        assert_eq!(texts(&n)[0], "```bash");
        assert_eq!(texts(&n)[2], "```");
        assert_eq!(texts(&n)[3], "```");
    }

    #[test]
    fn inline_triple_backticks_are_not_fences() {
        let n = norm("use ```ls``` here\n```echo hi```\n");
        assert!(n.lines.iter().all(|l| !l.code));
    }

    // ── headings ──

    #[test]
    fn hash_comments_inside_code_are_not_headings() {
        let h = heads("## Real\n\n```bash\n# comment\n\n# another\n## also a comment\nls\n```\n");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, "Real");
    }

    #[test]
    fn h1_is_a_heading_outside_code_but_not_inside_a_script() {
        let h = heads("intro\n\n# Real title\n\nbody\n\n# 3️⃣ Encrypt the vault\nsudo cryptsetup luksFormat vault.img\n");
        assert_eq!(h.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), vec!["Real title"]);
    }

    #[test]
    fn comments_in_an_unfenced_script_are_not_headings_even_when_the_commands_are_unknown() {
        let h = heads("## Script\n\n# Name of your VPN interface\nVPN_IF=\"wg0\"\n\n# Start VPN\nstart_vpn\n\n# Kill switch\nset -e\n");
        assert_eq!(h.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), vec!["Script"]);
    }

    #[test]
    fn h1_without_blank_lines_still_counts_next_to_a_heading_or_fence() {
        let h = heads("```bash\nls\n```\n# Cheat Sheet\n## Quick Reference\n\ntext\n");
        assert_eq!(h.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), vec!["Cheat Sheet", "Quick Reference"]);
    }

    #[test]
    fn banners_and_rule_lines() {
        let h = heads("# =====\n# TEXT PROCESSING\n# =====\n\n## Sub\n\n# ------\n\nbody\n");
        assert_eq!(h[0], (1, "TEXT PROCESSING".to_string(), HeadKind::Banner));
        assert_eq!(h.len(), 2, "bare rule lines are decoration");
    }

    #[test]
    fn a_trailing_hash_is_part_of_the_title_unless_it_is_a_closing_sequence() {
        assert_eq!(atx("# Learn C#"), Some((1, "Learn C#")));
        assert_eq!(atx("## Title ##"), Some((2, "Title")));
        assert_eq!(atx("#!/bin/bash"), None);
        assert_eq!(atx("#hashtag"), None);
        assert_eq!(atx("####### seven"), None);
    }

    #[test]
    fn list_items_a_converter_turned_into_headings_go_back_to_being_items() {
        let mut n = norm("Good systems use:\n\n## PBKDF2\n\nArgon2\n\nscrypt\n\nThese make brute-forcing expensive.\n");
        let scan = find_headings(&mut n.lines, &Vocab::seed());
        assert!(scan.heads.is_empty());
        assert_eq!(scan.demoted.len(), 1);
        assert!(n.lines.iter().any(|l| l.text == "- PBKDF2"));
    }

    #[test]
    fn a_flattened_table_of_headings_goes_back_to_being_a_list() {
        let t = "Option\nMeaning\n\n## AES-256-GCM\n\nauthenticated encryption\n\n## PBKDF2\n\nprotects password against brute force\n\niter 200000\nslows password cracking\n";
        let mut n = norm(t);
        let scan = find_headings(&mut n.lines, &Vocab::seed());
        assert!(scan.heads.is_empty(), "{:?}", scan.heads);
        assert_eq!(scan.demoted.len(), 2);
    }

    #[test]
    fn a_short_heading_with_a_one_line_description_stays_a_heading() {
        // Only one short description in a row: real headings, not a flattened list.
        let t = "## Install\n\nRun the installer\n\n## Usage\n\nPass the file name as the argument to the tool you just installed.\n";
        let h = heads(t);
        assert_eq!(h.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), vec!["Install", "Usage"]);
    }

    #[test]
    fn a_real_heading_after_an_intro_sentence_is_kept() {
        let h = heads("Quantifiers say how many times to match:\n\n### Basic Quantifiers\n\n| Symbol | Meaning |\n|---|---|\n");
        assert_eq!(h.len(), 1, "next line is a table, not a list item");
        let h = heads("Customize tmux like this:\n\n### Basic Configuration Example:\n\nCreate a file called .tmux.conf in your home directory.\n");
        assert_eq!(h.len(), 1, "next line is a sentence");
    }

    #[test]
    fn keycap_headings_nest_under_the_real_heading_above() {
        let h = heads("## Encryption\n\n1️⃣ Strong algorithm\n\nAES-256 is fine.\n\n2️⃣ Strong key derivation\n\nUse Argon2.\n");
        assert_eq!(
            h,
            vec![
                (2, "Encryption".to_string(), HeadKind::Atx),
                (3, "Strong algorithm".to_string(), HeadKind::Keycap),
                (3, "Strong key derivation".to_string(), HeadKind::Keycap),
            ]
        );
    }

    #[test]
    fn a_keycap_list_with_no_bodies_is_a_list_not_sections() {
        let h = heads("## Plan\n\n1️⃣ Restore tier one\n\n2️⃣ Restore workflow\n\n3️⃣ Print the index\n\nThen relax.\n");
        // only the last item has anything under it
        assert_eq!(h.iter().filter(|x| x.2 == HeadKind::Keycap).count(), 1);
        assert_eq!(h.last().unwrap().1, "Print the index");
    }

    #[test]
    fn consecutive_keycap_lines_are_a_list() {
        let h = heads("## Plan\n\n1️⃣ First\n2️⃣ Second\n3️⃣ Third\n");
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn step_lines_are_headings_when_they_stand_alone() {
        let h = heads("## Guide\n\nStep 14 — Scan suspicious files\n\nUse VirusTotal.\n\nStep 2: Install it\n\nRun apt.\n");
        assert_eq!(h.iter().filter(|x| x.2 == HeadKind::Step).count(), 2);
        assert_eq!(h[1].1, "Step 14 — Scan suspicious files");
        assert!(step("Step by step guide").is_none());
        assert!(step("Part of the problem is this").is_none());
        assert!(step("Stepping stones").is_none());
    }

    #[test]
    fn keycap_parsing() {
        assert_eq!(keycap("3\u{FE0F}\u{20E3} Strong passphrase"), Some("Strong passphrase"));
        assert_eq!(keycap("3\u{20E3} No selector"), Some("No selector"));
        assert_eq!(keycap("3 not a keycap"), None);
        assert_eq!(keycap("\u{1F51F} Ten"), Some("Ten"));
    }
}
