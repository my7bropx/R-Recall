//! The search language shared by the TUI, `recall search` and `recall pick`.
//!
//! ```text
//! nmap ping sweep            words → FTS prefix match, all must appear
//! "port scan"                a quoted phrase
//! tool:nmap  t:git           only entries about that tool
//! tag:recon  #recon          only entries carrying that tag
//! cat:cmd|note|tool          only that category
//! is:fav  is:danger          favorites / destructive commands
//! src:pack|user|import|tldr  where the entry came from
//! how do i kill a process    filler words are dropped automatically
//! ```

use crate::models::{Category, Source};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    pub raw: String,
    /// Free-text words as typed.
    pub terms: Vec<String>,
    /// `"quoted phrases"`.
    pub phrases: Vec<String>,
    pub tool: Option<String>,
    pub tags: Vec<String>,
    pub category: Option<Category>,
    pub favorite: bool,
    pub danger: bool,
    pub source: Option<Source>,
}

/// Words that carry no signal in a natural-language question. They are only
/// dropped when something else remains, so searching for "how" still works.
const STOPWORDS: &[&str] = &[
    "a", "an", "the", "of", "to", "do", "does", "i", "how", "what", "which", "is", "are", "can",
    "me", "my", "in", "on", "for", "with", "using", "use", "via", "from", "into", "and", "or", "it",
    "this", "that", "be", "should", "would", "want", "need", "command", "commands", "cmd",
];

fn tokenize(s: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut was_quoted = false;
    for c in s.chars() {
        match c {
            '"' => {
                if in_q {
                    in_q = false;
                    out.push((std::mem::take(&mut cur), true));
                    was_quoted = false;
                } else {
                    if !cur.is_empty() {
                        out.push((std::mem::take(&mut cur), false));
                    }
                    in_q = true;
                    was_quoted = true;
                }
            }
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push((std::mem::take(&mut cur), false));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        // An unterminated quote (mid-typing) is treated as a phrase so far.
        out.push((cur, was_quoted && in_q));
    }
    out
}

impl Query {
    pub fn parse(s: &str) -> Query {
        let mut q = Query { raw: s.to_string(), ..Default::default() };
        for (tok, quoted) in tokenize(s) {
            if quoted {
                if !tok.trim().is_empty() {
                    q.phrases.push(tok.trim().to_string());
                }
                continue;
            }
            if let Some(tag) = tok.strip_prefix('#') {
                if tag.chars().any(|c| c.is_alphanumeric()) {
                    q.tags.push(tag.to_lowercase());
                    continue;
                }
            }
            if let Some((k, v)) = tok.split_once(':') {
                let key = k.to_lowercase();
                let known = matches!(
                    key.as_str(),
                    "tool" | "t" | "cat" | "category" | "tag" | "is" | "src" | "source" | "from"
                );
                if known {
                    let v = v.trim();
                    if v.is_empty() {
                        continue; // still typing the value
                    }
                    match key.as_str() {
                        "tool" | "t" => q.tool = Some(v.to_lowercase()),
                        "tag" => q.tags.push(v.to_lowercase()),
                        "cat" | "category" => q.category = Category::parse(v),
                        "src" | "source" | "from" => q.source = Source::parse(v),
                        "is" => match v.to_lowercase().as_str() {
                            "fav" | "favorite" | "favourite" | "star" | "starred" => q.favorite = true,
                            "danger" | "dangerous" | "destructive" => q.danger = true,
                            _ => {}
                        },
                        _ => {}
                    }
                    continue;
                }
            }
            q.terms.push(tok);
        }
        q
    }

    pub fn from_words(words: &[String]) -> Query {
        Query::parse(&words.join(" "))
    }

    pub fn has_text(&self) -> bool {
        !self.terms.is_empty() || !self.phrases.is_empty()
    }

    pub fn has_filters(&self) -> bool {
        self.tool.is_some()
            || !self.tags.is_empty()
            || self.category.is_some()
            || self.favorite
            || self.danger
            || self.source.is_some()
    }

    pub fn is_empty(&self) -> bool {
        !self.has_text() && !self.has_filters()
    }

    /// The words that actually drive the search: filler removed unless that
    /// would leave nothing.
    pub fn effective_terms(&self) -> Vec<String> {
        let kept: Vec<String> = self
            .terms
            .iter()
            .filter(|t| !STOPWORDS.contains(&t.to_lowercase().as_str()))
            .cloned()
            .collect();
        if kept.is_empty() && self.phrases.is_empty() {
            self.terms.clone()
        } else {
            kept
        }
    }

    /// Lower-cased words to highlight in result titles.
    pub fn highlight_terms(&self) -> Vec<String> {
        let mut v: Vec<String> = self.effective_terms().iter().map(|t| t.to_lowercase()).collect();
        for p in &self.phrases {
            v.extend(p.split_whitespace().map(|w| w.to_lowercase()));
        }
        v.retain(|t| !t.is_empty());
        v
    }

    fn fts_token(term: &str, prefix: bool) -> Option<String> {
        if !term.chars().any(|c| c.is_alphanumeric()) {
            return None;
        }
        let esc = term.replace('"', "\"\"");
        Some(if prefix { format!("\"{}\"*", esc) } else { format!("\"{}\"", esc) })
    }

    /// Flag-shaped words (`-sV`, `--script`, `-p-`). The tokenizer drops the
    /// dashes, so these are matched as exact substrings instead of FTS terms.
    pub fn is_flag_term(t: &str) -> bool {
        t.starts_with('-') && t.chars().any(|c| c.is_alphanumeric())
    }

    pub fn flag_terms(&self) -> Vec<String> {
        self.terms.iter().filter(|t| Self::is_flag_term(t)).cloned().collect()
    }

    fn fts_parts(&self, terms: &[String]) -> Vec<String> {
        let mut parts: Vec<String> = terms
            .iter()
            .filter(|t| !Self::is_flag_term(t))
            .filter_map(|t| Self::fts_token(t, true))
            .collect();
        parts.extend(self.phrases.iter().filter_map(|p| Self::fts_token(p, false)));
        parts
    }

    /// FTS5 MATCH string requiring every word (implicit AND).
    pub fn fts_and(&self) -> Option<String> {
        self.fts_and_with(&self.effective_terms())
    }

    pub fn fts_and_with(&self, terms: &[String]) -> Option<String> {
        let parts = self.fts_parts(terms);
        if parts.is_empty() { None } else { Some(parts.join(" ")) }
    }

    /// FTS5 MATCH string accepting any word (ranking rewards matching more).
    pub fn fts_or(&self) -> Option<String> {
        let parts = self.fts_parts(&self.effective_terms());
        if parts.len() < 2 { None } else { Some(parts.join(" OR ")) }
    }
}

// ─── typo tolerance ──────────────────────────────────────────────────────────

/// Optimal-string-alignment distance (Levenshtein plus adjacent swaps), or
/// `None` when it exceeds `max`.
pub fn osa_distance(a: &str, b: &str, max: usize) -> Option<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > max {
        return None;
    }
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    (d[n][m] <= max).then_some(d[n][m])
}

/// How many edits to forgive for a word of this length.
pub fn typo_budget(len: usize) -> usize {
    match len {
        0..=2 => 0,
        3..=4 => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_words_become_terms() {
        let q = Query::parse("nmap ping sweep");
        assert_eq!(q.terms, vec!["nmap", "ping", "sweep"]);
        assert!(q.has_text() && !q.has_filters());
        assert_eq!(q.fts_and().unwrap(), "\"nmap\"* \"ping\"* \"sweep\"*");
        assert_eq!(q.fts_or().unwrap(), "\"nmap\"* OR \"ping\"* OR \"sweep\"*");
    }

    #[test]
    fn filters_are_extracted_and_not_searched_as_text() {
        let q = Query::parse("tool:NMAP scan #recon tag:web cat:cmd is:fav is:danger src:pack");
        assert_eq!(q.tool.as_deref(), Some("nmap"));
        assert_eq!(q.terms, vec!["scan"]);
        assert_eq!(q.tags, vec!["recon", "web"]);
        assert_eq!(q.category, Some(Category::Command));
        assert!(q.favorite && q.danger);
        assert_eq!(q.source, Some(Source::Pack));
    }

    #[test]
    fn half_typed_filters_are_ignored() {
        let q = Query::parse("tool: cat: scan");
        assert!(q.tool.is_none() && q.category.is_none());
        assert_eq!(q.terms, vec!["scan"]);
        let q = Query::parse("src:nonsense scan");
        assert!(q.source.is_none());
    }

    #[test]
    fn unknown_colon_tokens_are_ordinary_text() {
        let q = Query::parse("smb://host 10.0.0.1:445 http://x");
        assert_eq!(q.terms.len(), 3);
        assert!(!q.has_filters());
    }

    #[test]
    fn quoted_phrases_stay_together() {
        let q = Query::parse("\"port scan\" nmap");
        assert_eq!(q.phrases, vec!["port scan"]);
        assert_eq!(q.terms, vec!["nmap"]);
        assert_eq!(q.fts_and().unwrap(), "\"nmap\"* \"port scan\"");
        // unterminated quote while typing is tolerated
        let q = Query::parse("nmap \"port sc");
        assert_eq!(q.phrases, vec!["port sc"]);
    }

    #[test]
    fn stopwords_are_dropped_only_when_something_remains() {
        let q = Query::parse("how do i kill a process");
        assert_eq!(q.effective_terms(), vec!["kill", "process"]);
        let q = Query::parse("how to");
        assert_eq!(q.effective_terms(), vec!["how", "to"], "never end up with nothing");
    }

    #[test]
    fn punctuation_only_terms_are_not_fts_tokens() {
        let q = Query::parse("--- ~~~");
        assert!(q.fts_and().is_none());
        assert!(q.has_text(), "still text: the LIKE fallback handles it");
    }

    #[test]
    fn flag_shaped_words_are_substring_filters_not_fts_terms() {
        let q = Query::parse("nmap -sV --script -p-");
        assert_eq!(q.flag_terms(), vec!["-sV", "--script", "-p-"]);
        assert_eq!(q.fts_and().unwrap(), "\"nmap\"*", "only the real word goes to FTS");
        assert!(Query::parse("-p-").fts_and().is_none());
        assert!(!Query::is_flag_term("---"), "no alphanumerics: not a flag");
        assert!(!Query::is_flag_term("e-mail"), "dash must lead");
    }

    #[test]
    fn quotes_inside_terms_are_escaped() {
        let q = Query::parse("it's");
        assert_eq!(q.fts_and().unwrap(), "\"it's\"*");
        let m = Query::fts_token("a\"b", true).unwrap();
        assert_eq!(m, "\"a\"\"b\"*");
    }

    #[test]
    fn empty_and_filter_only_queries() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
        let q = Query::parse("tool:git");
        assert!(!q.is_empty() && !q.has_text() && q.has_filters());
    }

    #[test]
    fn osa_handles_typos_and_swaps() {
        assert_eq!(osa_distance("nmap", "nmap", 2), Some(0));
        assert_eq!(osa_distance("nmpa", "nmap", 2), Some(1), "adjacent swap costs 1");
        assert_eq!(osa_distance("gobsuter", "gobuster", 2), Some(1));
        assert_eq!(osa_distance("metasplot", "metasploit", 2), Some(1));
        assert_eq!(osa_distance("kitten", "sitting", 2), None);
        assert_eq!(osa_distance("abc", "abcdef", 2), None, "length gap short-circuits");
    }

    #[test]
    fn typo_budget_scales_with_length() {
        assert_eq!((typo_budget(2), typo_budget(4), typo_budget(8)), (0, 1, 2));
    }
}
