use crate::util::fnv_hex;

// ─── Category ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Category {
    Command,
    #[default]
    Note,
    Tool,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Command => "command",
            Category::Note    => "note",
            Category::Tool    => "tool",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "command" => Category::Command,
            "tool"    => Category::Tool,
            _         => Category::Note,
        }
    }

    /// Lenient parser for user-typed filters (`cat:cmd`, `--category tool`).
    pub fn parse(s: &str) -> Option<Category> {
        match s.trim().to_lowercase().as_str() {
            "command" | "cmd" | "commands" | "c" => Some(Category::Command),
            "note" | "notes" | "n"               => Some(Category::Note),
            "tool" | "tools" | "t"               => Some(Category::Tool),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Category::Command => "CMD",
            Category::Note    => "NOTE",
            Category::Tool    => "TOOL",
        }
    }

    pub fn cycle_next(&self) -> Category {
        match self {
            Category::Command => Category::Note,
            Category::Note    => Category::Tool,
            Category::Tool    => Category::Command,
        }
    }

    pub fn cycle_prev(&self) -> Category {
        match self {
            Category::Command => Category::Tool,
            Category::Note    => Category::Command,
            Category::Tool    => Category::Note,
        }
    }
}

// ─── Source (where an entry came from) ───────────────────────────────────────

/// Provenance. It drives ranking (your own entries beat bulk-imported ones) and
/// lets the built-in pack be updated without ever touching what you wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    /// Written by hand (TUI form, `recall add`).
    #[default]
    User,
    /// Bulk-imported from Markdown (`recall import` / `recall ingest`).
    Import,
    /// Shipped with the binary (the built-in knowledge pack).
    Pack,
    /// Imported from a local tldr-pages cache.
    Tldr,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::User   => "user",
            Source::Import => "import",
            Source::Pack   => "pack",
            Source::Tldr   => "tldr",
        }
    }

    /// Reading a stored value: anything unknown is treated as the user's own.
    pub fn from_db(s: &str) -> Source {
        match s {
            "import" => Source::Import,
            "pack"   => Source::Pack,
            "tldr"   => Source::Tldr,
            _        => Source::User,
        }
    }

    /// Lenient parser for filters (`src:pack`, `src:mine`).
    pub fn parse(s: &str) -> Option<Source> {
        match s.trim().to_lowercase().as_str() {
            "user" | "mine" | "own" | "me"        => Some(Source::User),
            "import" | "imported" | "notes"       => Some(Source::Import),
            "pack" | "builtin" | "built-in"       => Some(Source::Pack),
            "tldr"                               => Some(Source::Tldr),
            _ => None,
        }
    }

    /// Multiplier applied to search scores: the more "yours" an entry is, the
    /// more it is trusted. tldr is a large generic corpus, so it yields to
    /// curated material at equal relevance.
    pub fn prior(&self) -> f64 {
        match self {
            Source::User   => 1.15,
            Source::Pack   => 1.00,
            Source::Import => 0.90,
            Source::Tldr   => 0.75,
        }
    }
}

// ─── content hashing (pack sync uses this to detect user edits) ──────────────

#[allow(clippy::too_many_arguments)]
pub fn content_hash(
    title: &str,
    tool: &str,
    command: &str,
    content: &str,
    tags_csv: &str,
    keywords: &str,
    category: &str,
    danger: bool,
) -> String {
    fnv_hex(&[title, tool, command, content, tags_csv, keywords, category, if danger { "1" } else { "0" }])
}

// ─── fenced-block helpers ────────────────────────────────────────────────────

/// The inner text of every fenced code block in `text`, in order. An
/// unterminated final fence is still returned.
pub fn fenced_blocks(text: &str) -> Vec<String> {
    fenced_blocks_lang(text).into_iter().map(|(_, body)| body).collect()
}

/// Like [`fenced_blocks`], but with each block's info string (`bash`,
/// `python`, … lower-cased; empty when the fence has none).
pub fn fenced_blocks_lang(text: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, Vec<&str>)> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if let Some(info) = t.strip_prefix("```") {
            match cur.take() {
                Some((lang, lines)) => out.push((lang, lines.join("\n"))),
                None => {
                    let lang = info
                        .trim()
                        .split(|c: char| c.is_whitespace() || c == '{')
                        .next()
                        .unwrap_or("")
                        .to_lowercase();
                    cur = Some((lang, Vec::new()));
                }
            }
            continue;
        }
        if let Some((_, ref mut v)) = cur {
            v.push(line);
        }
    }
    if let Some((lang, lines)) = cur {
        out.push((lang, lines.join("\n")));
    }
    out
}

// ─── New / snapshot / stored entries ─────────────────────────────────────────

/// A not-yet-persisted entry (no id / timestamps / usage). Used by bulk
/// import, `recall add`, the ingest pipeline and the pack sync.
#[derive(Debug, Clone, Default)]
pub struct NewEntry {
    pub title:    String,
    pub content:  String,
    pub category: Category,
    pub tags:     Vec<String>,
    pub tool:     String,
    pub command:  String,
    pub keywords: String,
    pub danger:   bool,
    pub source:   Source,
    pub pack_key: String,
}

impl NewEntry {
    pub fn content_hash(&self) -> String {
        content_hash(
            &self.title,
            &self.tool,
            &self.command,
            &self.content,
            &self.tags.join(","),
            &self.keywords,
            self.category.as_str(),
            self.danger,
        )
    }
}

/// Snapshot of an entry, kept on the undo stack so `u` can restore it with its
/// original timestamps, usage counters and favorite flag intact.
#[derive(Debug, Clone)]
pub struct DeletedEntry {
    pub title:      String,
    pub content:    String,
    pub category:   Category,
    pub tags:       Vec<String>,
    pub favorite:   bool,
    pub created_at: String,
    pub updated_at: String,
    pub tool:       String,
    pub command:    String,
    pub keywords:   String,
    pub danger:     bool,
    pub uses:       i64,
    pub last_used:  String,
    pub source:     Source,
    pub pack_key:   String,
    pub pack_hash:  String,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id:         i64,
    pub title:      String,
    pub content:    String,
    pub category:   Category,
    pub tags:       Vec<String>,
    pub favorite:   bool,
    pub created_at: String,
    pub updated_at: String,
    /// The program this entry is about (`nmap`, `git`, …). Lower-case, may be empty.
    pub tool:       String,
    /// The copy-ready command. Empty for notes and for entries imported before
    /// this field existed (see [`Entry::primary_command`]).
    pub command:    String,
    /// Extra search words (synonyms, intent phrasing). Indexed, not displayed.
    pub keywords:   String,
    pub danger:     bool,
    pub uses:       i64,
    pub last_used:  String,
    pub source:     Source,
    pub pack_key:   String,
    pub pack_hash:  String,
}

impl Entry {
    pub fn tags_display(&self) -> String {
        self.tags.join(", ")
    }

    /// True when this entry carries its own dedicated command.
    pub fn has_command(&self) -> bool {
        !self.command.trim().is_empty()
    }

    /// The command a user wants on the clipboard: the dedicated `command`
    /// field, or — for older entries — the first fenced code block.
    pub fn primary_command(&self) -> String {
        if self.has_command() {
            return self.command.trim_end().to_string();
        }
        fenced_blocks(&self.content)
            .into_iter()
            .map(|b| b.trim().to_string())
            .find(|b| !b.is_empty())
            .unwrap_or_default()
    }

    /// The document shown in the preview and full view. Structured entries get
    /// their command rendered as a leading code block; older entries are shown
    /// as stored. Cursor movement, Visual selection and yank all operate on
    /// these lines, so what you select is exactly what you see.
    pub fn body(&self) -> String {
        let cmd = self.command.trim_end();
        if cmd.trim().is_empty() || self.content.contains(cmd.trim()) {
            return self.content.clone();
        }
        if self.content.trim().is_empty() {
            format!("```\n{}\n```", cmd)
        } else {
            format!("```\n{}\n```\n\n{}", cmd, self.content)
        }
    }

    /// Same hash [`NewEntry::content_hash`] computes, for the same fields.
    /// Not on the sync hot path (which reads columns straight from SQL rows),
    /// but kept and tested as the documented, symmetric way to compare an
    /// [`Entry`] against a [`NewEntry`] (e.g. from a REPL or a future tool).
    #[allow(dead_code)]
    pub fn content_hash(&self) -> String {
        content_hash(
            &self.title,
            &self.tool,
            &self.command,
            &self.content,
            &self.tags.join(","),
            &self.keywords,
            self.category.as_str(),
            self.danger,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(command: &str, content: &str) -> Entry {
        Entry {
            id: 1,
            title: "t".into(),
            content: content.into(),
            category: Category::Command,
            tags: vec![],
            favorite: false,
            created_at: String::new(),
            updated_at: String::new(),
            tool: String::new(),
            command: command.into(),
            keywords: String::new(),
            danger: false,
            uses: 0,
            last_used: String::new(),
            source: Source::User,
            pack_key: String::new(),
            pack_hash: String::new(),
        }
    }

    #[test]
    fn fenced_blocks_extracts_each_block() {
        let t = "intro\n```bash\nls -la\npwd\n```\ntext\n```\nwhoami\n```\n";
        assert_eq!(fenced_blocks(t), vec!["ls -la\npwd".to_string(), "whoami".to_string()]);
    }

    #[test]
    fn fenced_blocks_keeps_unterminated_tail() {
        assert_eq!(fenced_blocks("```\nonly"), vec!["only".to_string()]);
    }

    #[test]
    fn primary_command_prefers_dedicated_field() {
        assert_eq!(entry("nmap -p- x", "```\nother\n```").primary_command(), "nmap -p- x");
    }

    #[test]
    fn primary_command_falls_back_to_first_fenced_block() {
        assert_eq!(entry("", "some text\n```bash\nuniq file.txt\n```").primary_command(), "uniq file.txt");
        assert_eq!(entry("", "just prose").primary_command(), "");
    }

    #[test]
    fn body_prepends_command_only_when_not_already_in_content() {
        let e = entry("ss -tulpn", "Lists listening sockets.");
        assert_eq!(e.body(), "```\nss -tulpn\n```\n\nLists listening sockets.");

        let legacy = entry("uniq f", "```bash\nuniq f\n```");
        assert_eq!(legacy.body(), legacy.content, "no duplicate command block");

        assert_eq!(entry("ss -tulpn", "").body(), "```\nss -tulpn\n```");
    }

    #[test]
    fn hash_changes_with_any_field_and_matches_between_types() {
        let mut n = NewEntry {
            title: "a".into(),
            command: "b".into(),
            tags: vec!["x".into(), "y".into()],
            ..Default::default()
        };
        let h1 = n.content_hash();
        n.command = "c".into();
        assert_ne!(h1, n.content_hash());

        let mut e = entry("b", "");
        e.title = "a".into();
        e.tags = vec!["x".into(), "y".into()];
        e.category = Category::Note; // NewEntry's default category
        let n2 = NewEntry { title: "a".into(), command: "b".into(), tags: vec!["x".into(), "y".into()], ..Default::default() };
        assert_eq!(e.content_hash(), n2.content_hash(), "row hash and pack hash agree on identical data");
    }

    #[test]
    fn source_roundtrip_and_priors() {
        for s in [Source::User, Source::Import, Source::Pack, Source::Tldr] {
            assert_eq!(Source::from_db(s.as_str()), s);
        }
        assert!(Source::User.prior() > Source::Pack.prior());
        assert!(Source::Pack.prior() > Source::Tldr.prior());
        assert_eq!(Source::parse("builtin"), Some(Source::Pack));
        assert_eq!(Source::parse("nope"), None);
    }

    #[test]
    fn category_parse_accepts_aliases() {
        assert_eq!(Category::parse("CMD"), Some(Category::Command));
        assert_eq!(Category::parse("tools"), Some(Category::Tool));
        assert_eq!(Category::parse("x"), None);
    }
}
