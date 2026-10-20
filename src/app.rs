use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::{
    db::Database,
    dedupe::{self, DupKey, KeepRule},
    import::{self, ImportOptions},
    models::{Category, DeletedEntry, Entry},
    theme::Theme,
};

// ─── Screens & Vim modes ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Screen { List, View, Form }

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode { Normal, Insert, Command, Search, Visual, VisualLine }

impl Mode {
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Command => "COMMAND",
            Mode::Search => "SEARCH",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SortKey { Title, Updated, Created, Category, Favorite }

impl SortKey {
    pub fn label(&self) -> &'static str {
        match self {
            SortKey::Title => "title",
            SortKey::Updated => "updated",
            SortKey::Created => "created",
            SortKey::Category => "category",
            SortKey::Favorite => "favorite",
        }
    }
}

pub enum CmdOutcome { None, Quit, Editor }

/// One reversible change. Each variant carries enough state to be applied in
/// both directions, which is what makes redo possible.
pub enum Change {
    Delete { id: i64, snap: DeletedEntry },
    Edit { id: i64, prev: DeletedEntry, next: DeletedEntry },
    Favorite { id: i64, prev: bool, next: bool },
}

/// A batch of changes that undo/redo treat as a single step, so `:g/pat/d`
/// deleting 40 entries is one `u` away from coming back.
pub struct UndoGroup {
    pub label:   String,
    pub changes: Vec<Change>,
}

// ─── Form ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum FormField { Title, Category, Tags, Content }

impl FormField {
    pub fn next(&self) -> FormField {
        match self {
            FormField::Title => FormField::Category,
            FormField::Category => FormField::Tags,
            FormField::Tags => FormField::Content,
            FormField::Content => FormField::Title,
        }
    }
    pub fn prev(&self) -> FormField {
        match self {
            FormField::Title => FormField::Content,
            FormField::Category => FormField::Title,
            FormField::Tags => FormField::Category,
            FormField::Content => FormField::Tags,
        }
    }
}

pub struct FormState {
    pub title:      String,
    pub category:   Category,
    pub tags:       String,
    pub content:    String,
    pub focused:    FormField,
    pub editing_id: Option<i64>,
    /// Values as of the last open or save, for the unsaved-changes guard.
    orig: (String, Category, String, String),
}

impl FormState {
    pub fn new() -> Self {
        FormState {
            title: String::new(), category: Category::Command, tags: String::new(),
            content: String::new(), focused: FormField::Title, editing_id: None,
            orig: (String::new(), Category::Command, String::new(), String::new()),
        }
    }
    pub fn from_entry(entry: &Entry) -> Self {
        let tags = entry.tags.join(", ");
        FormState {
            title: entry.title.clone(), category: entry.category.clone(),
            tags: tags.clone(), content: entry.content.clone(),
            focused: FormField::Title, editing_id: Some(entry.id),
            orig: (entry.title.clone(), entry.category.clone(), tags, entry.content.clone()),
        }
    }
    /// True when the buffer differs from the last saved/opened state.
    pub fn is_dirty(&self) -> bool {
        self.title != self.orig.0
            || self.category != self.orig.1
            || self.tags != self.orig.2
            || self.content != self.orig.3
    }
    /// Mark the current values as the saved baseline.
    pub fn mark_clean(&mut self) {
        self.orig = (
            self.title.clone(),
            self.category.clone(),
            self.tags.clone(),
            self.content.clone(),
        );
    }
}

// ─── App ────────────────────────────────────────────────────────────────────

const JUMP: usize = 10;

pub struct App {
    pub db:          Database,
    pub screen:      Screen,
    pub mode:        Mode,
    pub all_entries: Vec<Entry>,
    pub id_index:    HashMap<i64, usize>, // id -> position in all_entries
    pub filtered:    Vec<usize>,          // indices into all_entries (no cloning)
    pub matches:     Vec<Vec<usize>>,     // title highlight char-indices, parallel to filtered
    pub selected:    usize,
    pub view_scroll: u16,
    pub view_height: u16,               // visible body rows in the full view, set on render
    pub cursor_line: usize,             // current line in the full view's content (0-based)
    pub cursor_col:  usize,             // current column within cursor_line (char-wise Visual)
    pub visual_anchor_line: usize,
    pub visual_anchor_col:  usize,
    pub search:      String,              // active filter text
    pub search_prev: String,              // backup while typing in Search mode
    pub last_search: String,              // last confirmed query (drives n/N after :noh)
    pub match_ids:   HashSet<i64>,        // ids matching last_search
    pub cmdline:     String,
    pub form:        FormState,
    pub status:      Option<String>,
    pub cat_filter:  Option<Category>,
    pub fav_filter:  bool,
    pub sort_key:    SortKey,
    pub sort_rev:    bool,
    pub pending:     Option<char>,
    pub help_open:   bool,
    pub undo_stack:  Vec<UndoGroup>,
    pub redo_stack:  Vec<UndoGroup>,
    pub theme:       Theme,
}

impl App {
    pub fn new(db: Database) -> Result<Self> {
        let all_entries = db.get_all_entries()?;
        let mut app = App {
            db,
            screen: Screen::List,
            mode: Mode::Normal,
            all_entries,
            id_index: HashMap::new(),
            filtered: Vec::new(),
            matches: Vec::new(),
            selected: 0,
            view_scroll: 0,
            view_height: 0,
            cursor_line: 0,
            cursor_col: 0,
            visual_anchor_line: 0,
            visual_anchor_col: 0,
            search: String::new(),
            search_prev: String::new(),
            last_search: String::new(),
            match_ids: HashSet::new(),
            cmdline: String::new(),
            form: FormState::new(),
            status: None,
            cat_filter: None,
            fav_filter: false,
            sort_key: SortKey::Updated,
            sort_rev: false,
            pending: None,
            help_open: false,
            undo_stack: Vec::new(),
            redo_stack: Vec::new(),
            theme: Theme::from_env(),
        };
        app.rebuild_index();
        app.apply_filter();
        Ok(app)
    }

    fn rebuild_index(&mut self) {
        self.id_index = self
            .all_entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id, i))
            .collect();
    }

    pub fn reload(&mut self) -> Result<()> {
        self.all_entries = self.db.get_all_entries()?;
        self.rebuild_index();
        self.apply_filter();
        if self.search.trim().is_empty() && !self.last_search.trim().is_empty() {
            self.match_ids = self
                .db
                .search_ids(&self.last_search)
                .unwrap_or_default()
                .into_iter()
                .collect();
        }
        Ok(())
    }

    fn sort_rows(&self, rows: &mut [usize]) {
        let a = &self.all_entries;
        match self.sort_key {
            SortKey::Title => rows.sort_by(|&x, &y| {
                a[x].title.to_lowercase().cmp(&a[y].title.to_lowercase())
            }),
            SortKey::Updated => rows.sort_by(|&x, &y| a[y].updated_at.cmp(&a[x].updated_at)),
            SortKey::Created => rows.sort_by(|&x, &y| a[y].created_at.cmp(&a[x].created_at)),
            SortKey::Category => rows.sort_by(|&x, &y| a[x].category.label().cmp(a[y].category.label())),
            SortKey::Favorite => rows.sort_by(|&x, &y| a[y].favorite.cmp(&a[x].favorite)),
        }
        if self.sort_rev {
            rows.reverse();
        }
    }

    pub fn apply_filter(&mut self) {
        let q   = self.search.trim().to_string();
        let cat = self.cat_filter.clone();
        let fav = self.fav_filter;
        let passes = |e: &Entry| -> bool {
            if let Some(ref c) = cat {
                if &e.category != c {
                    return false;
                }
            }
            if fav && !e.favorite {
                return false;
            }
            true
        };

        if q.is_empty() {
            let mut rows: Vec<usize> = self
                .all_entries
                .iter()
                .enumerate()
                .filter(|(_, e)| passes(e))
                .map(|(i, _)| i)
                .collect();
            self.sort_rows(&mut rows);
            self.matches = vec![Vec::new(); rows.len()];
            self.filtered = rows;
        } else {
            let ids = self.db.search_ids(&q).unwrap_or_default();
            self.last_search = q.clone();
            self.match_ids = ids.iter().copied().collect();

            let terms: Vec<Vec<char>> = q
                .split_whitespace()
                .map(|t| t.to_lowercase().chars().collect::<Vec<char>>())
                .filter(|v| !v.is_empty())
                .collect();

            let mut rows    = Vec::with_capacity(ids.len());
            let mut matches = Vec::with_capacity(ids.len());
            for id in &ids {
                if let Some(&i) = self.id_index.get(id) {
                    let e = &self.all_entries[i];
                    if passes(e) {
                        rows.push(i);
                        matches.push(title_match_indices(&e.title, &terms));
                    }
                }
            }
            self.matches = matches;
            self.filtered = rows;
        }

        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn selected_entry(&self) -> Option<&Entry> {
        self.filtered.get(self.selected).and_then(|&i| self.all_entries.get(i))
    }

    // list navigation
    pub fn move_up(&mut self)   { if self.selected > 0 { self.selected -= 1; } }
    pub fn move_down(&mut self) { if self.selected + 1 < self.filtered.len() { self.selected += 1; } }
    pub fn to_top(&mut self)    { self.selected = 0; }
    pub fn to_bottom(&mut self) { self.selected = self.filtered.len().saturating_sub(1); }
    pub fn half_up(&mut self)   { self.selected = self.selected.saturating_sub(JUMP); }
    pub fn half_down(&mut self) {
        self.selected = (self.selected + JUMP).min(self.filtered.len().saturating_sub(1));
    }

    // full-view scrolling / cursor — j/k, gg/G and Ctrl-d/u all move `cursor_line`,
    // which auto-scrolls into view. This is the same cursor that Visual and
    // Visual-Line selection extend from.
    pub fn view_down(&mut self, n: u16) { self.move_cursor_line(n as isize); }
    pub fn view_up(&mut self, n: u16)   { self.move_cursor_line(-(n as isize)); }
    pub fn view_top(&mut self) {
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.autoscroll();
    }
    pub fn view_bottom(&mut self) {
        let lines = self.content_lines();
        self.cursor_line = lines.len().saturating_sub(1);
        self.clamp_cursor_col(&lines);
        self.autoscroll();
    }

    /// The lines the full view and side preview actually render: `content`,
    /// with a synthesized command block in front when the entry keeps its
    /// command in the dedicated field rather than inline. Cursor movement,
    /// Visual selection and `yy`/line-yank all key off this, so what gets
    /// selected always matches what's on screen.
    fn content_lines(&self) -> Vec<String> {
        self.selected_entry().map(|e| e.body().lines().map(|l| l.to_string()).collect()).unwrap_or_default()
    }

    fn move_cursor_line(&mut self, delta: isize) {
        let lines = self.content_lines();
        if lines.is_empty() {
            return;
        }
        let max = (lines.len() - 1) as isize;
        let next = (self.cursor_line as isize + delta).clamp(0, max);
        self.cursor_line = next as usize;
        self.clamp_cursor_col(&lines);
        self.autoscroll();
    }

    fn clamp_cursor_col(&mut self, lines: &[String]) {
        match lines.get(self.cursor_line) {
            Some(l) => {
                let max = l.chars().count().saturating_sub(1);
                if self.cursor_col > max {
                    self.cursor_col = max;
                }
            }
            None => self.cursor_col = 0,
        }
    }

    pub fn cursor_left(&mut self) {
        self.cursor_col = self.cursor_col.saturating_sub(1);
    }
    pub fn cursor_right(&mut self) {
        if let Some(l) = self.content_lines().get(self.cursor_line) {
            let max = l.chars().count().saturating_sub(1);
            if self.cursor_col < max {
                self.cursor_col += 1;
            }
        }
    }

    /// Keep `cursor_line` inside the visible window recorded by the last render.
    fn autoscroll(&mut self) {
        let cl = self.cursor_line as u16;
        if cl < self.view_scroll {
            self.view_scroll = cl;
        } else if self.view_height > 0 && cl >= self.view_scroll + self.view_height {
            self.view_scroll = cl - self.view_height + 1;
        }
    }

    // ── Visual / Visual-Line selection (full view only) ─────────────────────

    pub fn enter_visual(&mut self) {
        self.visual_anchor_line = self.cursor_line;
        self.visual_anchor_col = self.cursor_col;
        self.mode = Mode::Visual;
    }
    pub fn enter_visual_line(&mut self) {
        self.visual_anchor_line = self.cursor_line;
        self.visual_anchor_col = 0;
        self.mode = Mode::VisualLine;
    }
    pub fn exit_visual(&mut self) {
        self.mode = Mode::Normal;
    }

    /// Yank the active Visual / Visual-Line selection to the system clipboard —
    /// exactly the selected text, no added leading/trailing lines or spaces.
    pub fn yank_visual(&mut self) {
        let text = self.visual_selection_text();
        let n = text.lines().count().max(1);
        self.copy_to_clipboard(&text, &format!("Yanked {} line{}", n, if n == 1 { "" } else { "s" }));
        self.exit_visual();
    }

    /// The exact text spanned by the current Visual / Visual-Line selection.
    fn visual_selection_text(&self) -> String {
        let lines = self.content_lines();
        if lines.is_empty() {
            return String::new();
        }
        match self.mode {
            Mode::VisualLine => {
                let (a, b) = ordered(self.visual_anchor_line, self.cursor_line);
                let b = b.min(lines.len() - 1);
                lines[a..=b].join("\n")
            }
            Mode::Visual => {
                let (start, end) = ordered_pos(
                    (self.visual_anchor_line, self.visual_anchor_col),
                    (self.cursor_line, self.cursor_col),
                );
                let (sl, sc) = start;
                let el = end.0.min(lines.len() - 1);
                let ec = end.1;
                if sl == el {
                    let chars: Vec<char> = lines[sl].chars().collect();
                    if chars.is_empty() {
                        String::new()
                    } else {
                        let e = ec.min(chars.len() - 1);
                        let s = sc.min(e);
                        chars[s..=e].iter().collect()
                    }
                } else {
                    let mut parts = Vec::with_capacity(el - sl + 1);
                    let first: Vec<char> = lines[sl].chars().collect();
                    let s = sc.min(first.len());
                    parts.push(first[s..].iter().collect::<String>());
                    for line in &lines[sl + 1..el] {
                        parts.push(line.clone());
                    }
                    let last: Vec<char> = lines[el].chars().collect();
                    parts.push(if last.is_empty() {
                        String::new()
                    } else {
                        let e = ec.min(last.len() - 1);
                        last[..=e].iter().collect()
                    });
                    parts.join("\n")
                }
            }
            _ => String::new(),
        }
    }

    // n / N — jump to next / previous entry matching last_search
    pub fn next_match(&mut self) {
        if self.match_ids.is_empty() || self.filtered.is_empty() {
            self.status = Some("No search matches".into());
            return;
        }
        let n = self.filtered.len();
        let mut pos = self.selected;
        for _ in 0..n {
            pos = (pos + 1) % n;
            if self.match_ids.contains(&self.all_entries[self.filtered[pos]].id) {
                self.selected = pos;
                return;
            }
        }
    }
    pub fn prev_match(&mut self) {
        if self.match_ids.is_empty() || self.filtered.is_empty() {
            self.status = Some("No search matches".into());
            return;
        }
        let n = self.filtered.len();
        let mut pos = self.selected;
        for _ in 0..n {
            pos = (pos + n - 1) % n;
            if self.match_ids.contains(&self.all_entries[self.filtered[pos]].id) {
                self.selected = pos;
                return;
            }
        }
    }

    // search mode
    pub fn start_search(&mut self) {
        self.mode = Mode::Search;
        self.search_prev = self.search.clone();
        self.cmdline = String::new();
        self.search = String::new();
        self.selected = 0;
        self.apply_filter();
    }
    pub fn search_push(&mut self, c: char) {
        self.cmdline.push(c);
        self.search = self.cmdline.clone();
        self.selected = 0;
        self.apply_filter();
    }
    pub fn search_pop(&mut self) {
        self.cmdline.pop();
        self.search = self.cmdline.clone();
        self.selected = 0;
        self.apply_filter();
    }
    pub fn search_confirm(&mut self) {
        self.mode = Mode::Normal;
        if self.search.is_empty() {
            self.status = None;
        } else {
            self.status = Some(format!("/{}  ({} matches)", self.search, self.filtered.len()));
        }
    }
    pub fn search_cancel(&mut self) {
        self.search = self.search_prev.clone();
        self.cmdline.clear();
        self.mode = Mode::Normal;
        self.apply_filter();
    }

    // command mode
    pub fn start_command(&mut self) {
        self.mode = Mode::Command;
        self.cmdline = String::new();
    }
    pub fn cmd_push(&mut self, c: char) { self.cmdline.push(c); }
    pub fn cmd_pop(&mut self)           { self.cmdline.pop(); }

    // screens
    pub fn open_view(&mut self) {
        if self.selected_entry().is_some() {
            self.view_scroll = 0;
            self.cursor_line = 0;
            self.cursor_col = 0;
            self.screen = Screen::View;
            self.mode = Mode::Normal;
        }
    }
    pub fn new_form(&mut self) {
        self.form = FormState::new();
        self.screen = Screen::Form;
        self.mode = Mode::Insert;
    }
    pub fn edit_form(&mut self) {
        if let Some(e) = self.selected_entry() {
            self.form = FormState::from_entry(e);
            self.screen = Screen::Form;
            self.mode = Mode::Insert;
        } else {
            self.status = Some("Nothing selected".into());
        }
    }
    pub fn cancel_form(&mut self) {
        self.screen = Screen::List;
        self.mode = Mode::Normal;
    }

    /// Close the form, refusing if there are unsaved changes (Vim's E37).
    /// Returns true when the form was actually closed.
    pub fn try_cancel_form(&mut self, force: bool) -> bool {
        if !force && self.form.is_dirty() {
            self.status =
                Some("No write since last change — :w to save, :q! to discard".into());
            return false;
        }
        self.cancel_form();
        true
    }

    pub fn persist_form(&mut self) -> Result<bool> {
        let title = self.form.title.trim().to_string();
        if title.is_empty() {
            self.status = Some("Title cannot be empty".into());
            return Ok(false);
        }
        let content = self.form.content.clone();
        let tags: Vec<String> = self
            .form
            .tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // The form has no Tool/Command field, so it can't show or edit those.
        // An entry that already carries a dedicated command (pack-synced or
        // made with `recall add --command`) keeps it and its tool/keywords
        // untouched; only title/content/category/tags change here. An entry
        // with no dedicated command — the common case for older, freeform
        // entries — gets tool/command/danger (re)derived from the new content,
        // same as the CLI's `add` does.
        let prior = self.form.editing_id.and_then(|id| self.id_index.get(&id)).map(|&i| &self.all_entries[i]);
        let (tool, command, keywords, danger) = match prior {
            Some(p) if p.has_command() => (p.tool.clone(), p.command.clone(), p.keywords.clone(), p.danger),
            _ => {
                let tool = crate::derive::derive_tool(&title, &content, &tags);
                let command = if self.form.category == Category::Command {
                    crate::models::fenced_blocks(&content).into_iter().find(|b| !b.trim().is_empty()).unwrap_or_default()
                } else {
                    String::new()
                };
                let danger = crate::danger::danger_reason(&command).is_some();
                (tool, command, String::new(), danger)
            }
        };
        let new_entry = crate::models::NewEntry {
            title: title.clone(),
            content: content.clone(),
            category: self.form.category.clone(),
            tags: tags.clone(),
            tool,
            command,
            keywords,
            danger,
            source: crate::models::Source::User,
            pack_key: String::new(),
        };

        if let Some(id) = self.form.editing_id {
            // Snapshot the pre-edit row so `u` can put it back.
            let prev = self.id_index.get(&id).map(|&i| snapshot(&self.all_entries[i]));
            self.db.update_full(id, &new_entry)?;
            if let Some(prev) = prev {
                let changed = prev.title != title
                    || prev.content != content
                    || prev.category != self.form.category
                    || prev.tags != tags
                    || prev.command != new_entry.command
                    || prev.tool != new_entry.tool;
                if changed {
                    // Re-read the row so redo can replay the exact saved state.
                    let next = self
                        .db
                        .get_entry(id)?
                        .map(|e| snapshot(&e))
                        .unwrap_or_else(|| prev.clone());
                    self.push_undo(UndoGroup {
                        label: format!("edit \"{}\"", title),
                        changes: vec![Change::Edit { id, prev, next }],
                    });
                }
            }
            self.status = Some(format!("Written: {}", title));
        } else {
            let id = self.db.add_new(&new_entry)?;
            self.form.editing_id = Some(id);
            self.status = Some(format!("Written (new): {}", title));
        }
        self.form.mark_clean();
        self.reload()?;
        Ok(true)
    }

    fn push_undo(&mut self, group: UndoGroup) {
        self.undo_stack.push(group);
        if self.undo_stack.len() > 200 {
            self.undo_stack.remove(0);
        }
        // A fresh change invalidates the redo branch, as in Vim.
        self.redo_stack.clear();
    }

    /// Does this entry pass the active category / favorites filters? `:g`
    /// operates on the visible buffer, so it uses the same predicate.
    fn passes_filters(&self, e: &Entry) -> bool {
        if let Some(ref c) = self.cat_filter {
            if &e.category != c {
                return false;
            }
        }
        if self.fav_filter && !e.favorite {
            return false;
        }
        true
    }

    pub fn delete_selected(&mut self) -> Result<()> {
        if let Some(entry) = self.selected_entry() {
            let snap  = snapshot(entry);
            let id    = entry.id;
            let title = entry.title.clone();
            self.db.delete_entry(id)?;
            self.push_undo(UndoGroup {
                label: format!("delete \"{}\"", title),
                changes: vec![Change::Delete { id, snap }],
            });
            self.status = Some(format!("Deleted: {} · u to undo", title));
        }
        self.reload()?;
        if !self.filtered.is_empty() && self.selected >= self.filtered.len() {
            self.selected = self.filtered.len() - 1;
        }
        Ok(())
    }

    // ── undo / redo ───────────────────────────────────────────────────────

    /// Reverse one change. Returns the id to select afterwards, if any.
    fn undo_change(&mut self, c: &mut Change) -> Result<Option<i64>> {
        Ok(match c {
            Change::Delete { id, snap } => {
                let nid = self.db.restore_entry(snap)?;
                *id = nid; // re-insert gets a fresh rowid; keep redo pointing at it
                Some(nid)
            }
            Change::Edit { id, prev, .. } => {
                if self.db.restore_edit(*id, prev)? == 0 {
                    let nid = self.db.restore_entry(prev)?;
                    *id = nid;
                    Some(nid)
                } else {
                    Some(*id)
                }
            }
            Change::Favorite { id, prev, .. } => {
                self.db.set_favorite(*id, *prev)?;
                Some(*id)
            }
        })
    }

    /// Re-apply one change.
    fn redo_change(&mut self, c: &mut Change) -> Result<Option<i64>> {
        Ok(match c {
            Change::Delete { id, .. } => {
                self.db.delete_entry(*id)?;
                None
            }
            Change::Edit { id, next, .. } => {
                if self.db.restore_edit(*id, next)? == 0 {
                    let nid = self.db.restore_entry(next)?;
                    *id = nid;
                    Some(nid)
                } else {
                    Some(*id)
                }
            }
            Change::Favorite { id, next, .. } => {
                self.db.set_favorite(*id, *next)?;
                Some(*id)
            }
        })
    }

    pub fn undo(&mut self) -> Result<()> {
        let mut group = match self.undo_stack.pop() {
            Some(g) => g,
            None => {
                self.status = Some("Already at oldest change".into());
                return Ok(());
            }
        };
        let mut sel = None;
        // Reverse order: the last change made is the first undone.
        for c in group.changes.iter_mut().rev() {
            if let Some(id) = self.undo_change(c)? {
                sel = Some(id);
            }
        }
        let n = group.changes.len();
        self.reload()?;
        if let Some(id) = sel {
            self.select_id(id);
        }
        self.status = Some(format!(
            "Undo: {}{}",
            group.label,
            if n > 1 { format!(" ({} entries)", n) } else { String::new() }
        ));
        self.redo_stack.push(group);
        Ok(())
    }

    pub fn redo(&mut self) -> Result<()> {
        let mut group = match self.redo_stack.pop() {
            Some(g) => g,
            None => {
                self.status = Some("Already at newest change".into());
                return Ok(());
            }
        };
        let mut sel = None;
        for c in group.changes.iter_mut() {
            if let Some(id) = self.redo_change(c)? {
                sel = Some(id);
            }
        }
        let n = group.changes.len();
        self.reload()?;
        if let Some(id) = sel {
            self.select_id(id);
        }
        self.status = Some(format!(
            "Redo: {}{}",
            group.label,
            if n > 1 { format!(" ({} entries)", n) } else { String::new() }
        ));
        // Redo must not clear its own branch, so push directly.
        self.undo_stack.push(group);
        Ok(())
    }

    // ── :g/pattern/cmd — bulk operations over the visible buffer ────────────

    /// Ids matching `pattern` that also pass the active tab / favorites filter.
    fn global_targets(&self, pattern: &str) -> Vec<i64> {
        self.db
            .search_ids(pattern)
            .unwrap_or_default()
            .into_iter()
            .filter(|id| {
                self.id_index
                    .get(id)
                    .map(|&i| self.passes_filters(&self.all_entries[i]))
                    .unwrap_or(false)
            })
            .collect()
    }

    pub fn bulk_delete(&mut self, pattern: &str) -> Result<()> {
        let ids = self.global_targets(pattern);
        if ids.is_empty() {
            self.status = Some(format!("No entries match /{}/", pattern));
            return Ok(());
        }
        let mut changes = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(&i) = self.id_index.get(&id) {
                let snap = snapshot(&self.all_entries[i]);
                self.db.delete_entry(id)?;
                changes.push(Change::Delete { id, snap });
            }
        }
        let n = changes.len();
        self.push_undo(UndoGroup { label: format!("delete /{}/", pattern), changes });
        self.reload()?;
        self.status = Some(format!("Deleted {} entries matching /{}/ · u to undo", n, pattern));
        Ok(())
    }

    pub fn bulk_favorite(&mut self, pattern: &str, on: bool) -> Result<()> {
        let ids = self.global_targets(pattern);
        let mut changes = Vec::new();
        for id in ids {
            if let Some(&i) = self.id_index.get(&id) {
                let prev = self.all_entries[i].favorite;
                if prev == on {
                    continue; // already in the wanted state
                }
                self.db.set_favorite(id, on)?;
                changes.push(Change::Favorite { id, prev, next: on });
            }
        }
        if changes.is_empty() {
            self.status = Some(format!("Nothing to change for /{}/", pattern));
            return Ok(());
        }
        let n = changes.len();
        let verb = if on { "favorite" } else { "unfavorite" };
        self.push_undo(UndoGroup { label: format!("{} /{}/", verb, pattern), changes });
        self.reload()?;
        self.status = Some(format!("{}d {} entries · u to undo", verb, n));
        Ok(())
    }

    /// `:dedupe` reports; `:dedupe!` applies. Applying goes through the undo
    /// stack, so the whole cleanup is one `u` away from being reversed.
    pub fn dedupe(&mut self, key: DupKey, keep: KeepRule, apply: bool) -> Result<()> {
        let plan  = dedupe::plan(&self.all_entries, key, keep, true);
        let dupes = dedupe::removable(&plan);

        if plan.is_empty() {
            self.status = Some(format!(
                "No duplicates among {} entries (key: {})",
                self.all_entries.len(),
                key.label()
            ));
            return Ok(());
        }
        if !apply {
            self.status = Some(format!(
                "{} groups · {} removable (key: {}, keep: {}) — :dedupe! to apply",
                plan.len(),
                dupes,
                key.label(),
                keep.label()
            ));
            return Ok(());
        }

        let mut changes: Vec<Change> = Vec::new();
        for g in &plan {
            // merge tags/favorite into the survivor, recorded as a reversible edit
            if g.survivor_changed {
                if let Some(&i) = self.id_index.get(&g.survivor) {
                    let prev = snapshot(&self.all_entries[i]);
                    self.db.update_entry(
                        g.survivor,
                        &prev.title,
                        &prev.content,
                        prev.category.clone(),
                        &g.merged_tags,
                    )?;
                    self.db.set_favorite(g.survivor, g.favorite)?;
                    let next = self
                        .db
                        .get_entry(g.survivor)?
                        .map(|e| snapshot(&e))
                        .unwrap_or_else(|| prev.clone());
                    changes.push(Change::Edit { id: g.survivor, prev, next });
                }
            }
            for v in &g.victims {
                if let Some(&i) = self.id_index.get(v) {
                    let snap = snapshot(&self.all_entries[i]);
                    self.db.delete_entry(*v)?;
                    changes.push(Change::Delete { id: *v, snap });
                }
            }
        }

        let removed = changes.iter().filter(|c| matches!(c, Change::Delete { .. })).count();
        self.push_undo(UndoGroup { label: format!("dedupe ({})", key.label()), changes });
        self.reload()?;
        self.status = Some(format!("Removed {} duplicates · u to undo", removed));
        Ok(())
    }

    fn cmd_dedupe(&mut self, args: &[&str], apply: bool) -> Result<()> {
        let mut key  = DupKey::Exact;
        let mut keep = KeepRule::Longest;
        for a in args {
            if let Some(k) = DupKey::parse(a) {
                key = k;
            } else if let Some(k) = KeepRule::parse(a) {
                keep = k;
            } else {
                self.status = Some(format!(
                    "Unknown :dedupe option '{}' (exact|title|normalized, first|last|longest)",
                    a
                ));
                return Ok(());
            }
        }
        self.dedupe(key, keep, apply)
    }

    /// Parse and run `g/pattern/cmd`.
    fn cmd_global(&mut self, rest: &str) -> Result<()> {
        let cut = match rest.rfind('/') {
            Some(i) => i,
            None => {
                self.status = Some("Usage: :g/pattern/d  (also fav, unfav)".into());
                return Ok(());
            }
        };
        let pattern = rest[..cut].trim().to_string();
        let action  = rest[cut + 1..].trim().to_string();
        if pattern.is_empty() {
            self.status = Some("Empty pattern".into());
            return Ok(());
        }
        match action.as_str() {
            "d" | "delete" | "del" => self.bulk_delete(&pattern)?,
            "fav" | "favorite"     => self.bulk_favorite(&pattern, true)?,
            "unfav" | "unfavorite" => self.bulk_favorite(&pattern, false)?,
            other => {
                self.status = Some(format!("Unknown :g action '{}' (d | fav | unfav)", other))
            }
        }
        Ok(())
    }

    /// Move the selection to the entry with this id, if it's in the current list.
    fn select_id(&mut self, id: i64) {
        if let Some(pos) = self.filtered.iter().position(|&i| self.all_entries[i].id == id) {
            self.selected = pos;
        }
    }

    pub fn toggle_favorite_selected(&mut self) -> Result<()> {
        if let Some(entry) = self.selected_entry() {
            let id    = entry.id;
            let prev  = entry.favorite;
            let nf    = !prev;
            let title = entry.title.clone();
            self.db.set_favorite(id, nf)?;
            self.push_undo(UndoGroup {
                label: format!("{} \"{}\"", if nf { "favorite" } else { "unfavorite" }, title),
                changes: vec![Change::Favorite { id, prev, next: nf }],
            });
            self.status = Some(format!(
                "{} {}",
                if nf { "★ favorited" } else { "☆ unfavorited" },
                title
            ));
        }
        self.reload()?;
        Ok(())
    }

    pub fn yank_selected(&mut self) {
        if let Some(entry) = self.selected_entry() {
            let content = entry.content.clone();
            self.copy_to_clipboard(&content, "Yanked");
        }
    }

    /// `Y` — copy just the ready-to-run command (placeholders silently filled
    /// from pinned variables and safe built-ins; anything left open stays as
    /// written). For a note with no command this falls back to `yy`'s
    /// whole-content yank.
    pub fn yank_command_selected(&mut self) {
        let Some(entry) = self.selected_entry() else { return };
        if !entry.has_command() {
            self.yank_selected();
            return;
        }
        let ctx = self.fill_ctx();
        let (text, unresolved) = crate::fill::fill_quiet(&entry.primary_command(), &HashMap::new(), &ctx);
        let id = entry.id;
        let label = if unresolved.is_empty() {
            "Yanked command".to_string()
        } else {
            format!("Yanked command (left as-is: {})", unresolved.join(", "))
        };
        self.copy_to_clipboard(&text, &label);
        let _ = self.db.record_use(id);
    }

    fn fill_ctx(&self) -> crate::fill::Ctx {
        let mut ctx = crate::fill::Ctx::default();
        if let Ok(vars) = self.db.vars() {
            for v in vars {
                ctx.last.insert(v.name.clone(), v.value.clone());
                if v.pinned {
                    ctx.pinned.insert(v.name, v.value);
                }
            }
        }
        ctx
    }

    /// Yank just the line the cursor is on (Vim's `yy`) — unlike
    /// `yank_selected`, which always grabs the whole entry regardless of
    /// where the cursor is.
    pub fn yank_current_line(&mut self) {
        if let Some(line) = self.content_lines().get(self.cursor_line) {
            let text = line.clone();
            self.copy_to_clipboard(&text, "Yanked 1 line");
        }
    }

    /// Copy to the system clipboard (xclip → xsel → wl-copy, first found).
    /// This is the app's only clipboard register, so it doubles as Vim's `+y`.
    fn copy_to_clipboard(&mut self, text: &str, label: &str) {
        self.status = Some(match crate::clipboard::copy(text) {
            Some(tool) => format!("{} via {}", label, tool),
            None => crate::clipboard::MISSING_HINT.to_string(),
        });
    }

    // filters
    pub fn set_cat_filter(&mut self, cat: Option<Category>) {
        self.cat_filter = cat;
        self.selected = 0;
        self.apply_filter();
    }

    /// The tab order shown in the header: ALL → CMD → NOTE → TOOL → FAV → ALL.
    /// Each entry is (category filter, favorites-only) — FAV shows every
    /// category's favorites, same as `:favorites`.
    fn tab_order() -> [(Option<Category>, bool); 5] {
        [
            (None, false),
            (Some(Category::Command), false),
            (Some(Category::Note), false),
            (Some(Category::Tool), false),
            (None, true),
        ]
    }

    fn tab_label(pos: &(Option<Category>, bool)) -> &'static str {
        match pos {
            (_, true) => "FAV",
            (None, false) => "ALL",
            (Some(Category::Command), false) => "CMD",
            (Some(Category::Note), false) => "NOTE",
            (Some(Category::Tool), false) => "TOOL",
        }
    }

    fn cycle_tab(&mut self, forward: bool) {
        let order = Self::tab_order();
        let cur = order
            .iter()
            .position(|p| p.0 == self.cat_filter && p.1 == self.fav_filter)
            .unwrap_or(0);
        let next = if forward {
            (cur + 1) % order.len()
        } else {
            (cur + order.len() - 1) % order.len()
        };
        let target = order[next].clone();
        let label = Self::tab_label(&target);
        self.cat_filter = target.0;
        self.fav_filter = target.1;
        self.selected = 0;
        self.apply_filter();
        self.status = Some(format!("Tab: {}  ({} entries)", label, self.filtered.len()));
    }

    /// `gt` — next category tab.
    pub fn next_tab(&mut self) { self.cycle_tab(true); }
    /// `gT` — previous category tab.
    pub fn prev_tab(&mut self) { self.cycle_tab(false); }

    /// Arrow keys are intentionally inert; nudge toward the home row instead.
    pub fn arrow_hint(&mut self) {
        self.status = Some("Arrows are disabled — use h j k l".into());
    }

    pub fn clear_search_filter(&mut self) {
        if !self.search.is_empty() {
            self.search.clear();
            self.selected = 0;
            self.apply_filter(); // keeps last_search / match_ids so n/N still works
            self.status = Some("Cleared search filter".into());
        }
    }

    // `:` command dispatch
    pub fn execute_command(&mut self, raw: &str) -> Result<CmdOutcome> {
        let raw = raw.trim();
        // `:g/pattern/cmd` is parsed whole — the pattern may contain spaces.
        if let Some(rest) = raw.strip_prefix("g/").or_else(|| raw.strip_prefix("global/")) {
            self.cmd_global(rest)?;
            return Ok(CmdOutcome::None);
        }
        let tokens: Vec<&str> = raw.split_whitespace().collect();
        if tokens.is_empty() {
            return Ok(CmdOutcome::None);
        }
        let bang = tokens[0].ends_with('!');
        let name = tokens[0].trim_end_matches('!');

        match name {
            "w" | "write" => {
                if self.screen == Screen::Form {
                    self.persist_form()?;
                } else {
                    self.status = Some("Not editing (:w works inside a form)".into());
                }
            }
            "q" | "quit" => match self.screen {
                Screen::Form => { self.try_cancel_form(bang); }
                Screen::View => self.screen = Screen::List,
                Screen::List => return Ok(CmdOutcome::Quit),
            },
            "wq" | "x" => {
                if self.screen == Screen::Form {
                    if self.persist_form()? {
                        self.cancel_form();
                    }
                } else {
                    return Ok(CmdOutcome::Quit);
                }
            }
            "sort" => self.cmd_sort(&tokens[1..], bang),
            "d" | "delete" | "del" => self.delete_selected()?,
            "u" | "undo" => self.undo()?,
            "red" | "redo" => self.redo()?,
            "dedupe" | "dedup" | "dupes" => self.cmd_dedupe(&tokens[1..], bang)?,
            "theme" => self.cmd_theme(tokens.get(1).copied()),
            "new" | "add" => self.new_form(),
            "e" | "edit" => self.edit_form(),
            "cat" | "filter" => self.cmd_cat(tokens.get(1).copied()),
            "fav" | "favorite" => self.toggle_favorite_selected()?,
            "favorites" | "favs" => {
                self.fav_filter = !self.fav_filter;
                self.selected = 0;
                self.apply_filter();
                self.status = Some(
                    if self.fav_filter { "Showing favorites only" } else { "Showing all entries" }.into(),
                );
            }
            "noh" | "nohl" | "nohlsearch" => {
                self.search.clear();
                self.apply_filter();
            }
            "help" | "h" => self.help_open = true,
            "editor" | "ed" => {
                if self.screen == Screen::Form {
                    return Ok(CmdOutcome::Editor);
                } else {
                    self.status = Some("Editor opens the content field inside a form".into());
                }
            }
            "import" => {
                if let Some(p) = tokens.get(1) {
                    self.cmd_import(p)?;
                } else {
                    self.status = Some("Usage: :import <path>".into());
                }
            }
            "set" => {
                if tokens.len() < 3 {
                    self.status = Some("Usage: :set <name> <value>".into());
                } else {
                    let name = crate::fill::canon(tokens[1]);
                    let value = tokens[2..].join(" ");
                    self.db.set_var(&name, &value)?;
                    self.status = Some(format!("{} = {}", name, value));
                }
            }
            "unset" => {
                if let Some(n) = tokens.get(1) {
                    let name = crate::fill::canon(n);
                    let removed = self.db.unset_var(&name)?;
                    self.status =
                        Some(if removed { format!("unset {}", name) } else { format!("(not set: {})", name) });
                } else {
                    self.status = Some("Usage: :unset <name>".into());
                }
            }
            "vars" => {
                let vars = self.db.vars()?;
                self.status = Some(if vars.is_empty() {
                    "No variables pinned — :set target 10.10.11.5".to_string()
                } else {
                    vars.iter()
                        .map(|v| format!("{}={}{}", v.name, v.value, if v.pinned { "" } else { "*" }))
                        .collect::<Vec<_>>()
                        .join("  ")
                });
            }
            other => self.status = Some(format!("Unknown command: :{}", other)),
        }
        Ok(CmdOutcome::None)
    }

    fn cmd_sort(&mut self, args: &[&str], bang: bool) {
        let mut rev = bang;
        let mut chosen = None;
        for a in args {
            match *a {
                "!" => rev = true,
                "title" | "name" => chosen = Some(SortKey::Title),
                "updated" | "modified" => chosen = Some(SortKey::Updated),
                "created" => chosen = Some(SortKey::Created),
                "category" | "cat" => chosen = Some(SortKey::Category),
                "favorite" | "fav" | "favs" => chosen = Some(SortKey::Favorite),
                other => {
                    self.status = Some(format!("Unknown sort key: {}", other));
                    return;
                }
            }
        }
        self.sort_key = chosen.unwrap_or(SortKey::Title);
        self.sort_rev = rev;
        self.selected = 0;
        self.apply_filter();
        self.status = Some(format!(
            "Sorted by {}{}",
            self.sort_key.label(),
            if rev { " (reversed)" } else { "" }
        ));
    }

    fn cmd_cat(&mut self, arg: Option<&str>) {
        match arg {
            None | Some("all") | Some("*") => self.set_cat_filter(None),
            Some("command") | Some("cmd") => self.set_cat_filter(Some(Category::Command)),
            Some("note") => self.set_cat_filter(Some(Category::Note)),
            Some("tool") => self.set_cat_filter(Some(Category::Tool)),
            Some(x) => self.status = Some(format!("Unknown category: {} (all|command|note|tool)", x)),
        }
    }

    fn cmd_theme(&mut self, arg: Option<&str>) {
        match arg {
            None => {
                self.status = Some(format!(
                    "theme: {} ({}) · available: {}",
                    self.theme.name,
                    if self.theme.rgb { "truecolor" } else { "ansi" },
                    Theme::names()
                ))
            }
            Some(name) => match Theme::by_name(name) {
                Some(t) => {
                    // Keep the detected color mode; only the palette changes.
                    self.theme = t.with_color_mode(self.theme.rgb);
                    self.status = Some(format!("theme: {}", self.theme.name));
                }
                None => {
                    self.status =
                        Some(format!("Unknown theme '{}' — try: {}", name, Theme::names()))
                }
            },
        }
    }

    fn cmd_import(&mut self, path: &str) -> Result<()> {
        let opts = ImportOptions { dry_run: false, flagged_only: false, extra_tags: vec![] };
        match import::import_markdown(&mut self.db, Path::new(path), &opts) {
            Ok(s) => {
                self.reload()?;
                self.status = Some(format!(
                    "Imported {} ({} skipped, {} duplicates) from {}",
                    s.candidates, s.skipped, s.duplicates, path
                ));
            }
            Err(e) => self.status = Some(format!("Import failed: {}", e)),
        }
        Ok(())
    }
}

/// Sort a pair so the first value is never greater than the second.
fn ordered(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Same as `ordered`, but for (line, col) positions compared lexicographically.
fn ordered_pos(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Capture an entry's restorable state.
fn snapshot(e: &Entry) -> DeletedEntry {
    DeletedEntry {
        title:      e.title.clone(),
        content:    e.content.clone(),
        category:   e.category.clone(),
        tags:       e.tags.clone(),
        favorite:   e.favorite,
        created_at: e.created_at.clone(),
        updated_at: e.updated_at.clone(),
        tool:       e.tool.clone(),
        command:    e.command.clone(),
        keywords:   e.keywords.clone(),
        danger:     e.danger,
        uses:       e.uses,
        last_used:  e.last_used.clone(),
        source:     e.source,
        pack_key:   e.pack_key.clone(),
        pack_hash:  e.pack_hash.clone(),
    }
}

/// Char-indices in `title` covered by any of the (lowercased) search terms,
/// for highlighting. Cheap: titles are short.
fn title_match_indices(title: &str, terms: &[Vec<char>]) -> Vec<usize> {
    let tl: Vec<char> = title.to_lowercase().chars().collect();
    if terms.is_empty() || tl.is_empty() {
        return Vec::new();
    }
    let mut hit = vec![false; tl.len()];
    for term in terms {
        let n = term.len();
        if n == 0 || n > tl.len() {
            continue;
        }
        let mut i = 0;
        while i + n <= tl.len() {
            if tl[i..i + n] == term[..] {
                for k in i..i + n {
                    hit[k] = true;
                }
                i += n;
            } else {
                i += 1;
            }
        }
    }
    hit.iter().enumerate().filter_map(|(i, &b)| b.then_some(i)).collect()
}

// ─── headless logic tests (no TTY needed) ──────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;
    use std::path::PathBuf;

    fn tmp_db() -> (Database, PathBuf) {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("recall_test_{}_{}.db", std::process::id(), nanos));
        let _ = std::fs::remove_file(&p);
        (Database::new(&p).unwrap(), p)
    }

    fn seed(db: &Database) {
        db.add_entry("kerberoast attack", "use GetUserSPNs", Category::Command, &["ad".into()]).unwrap();
        db.add_entry("luks disk encryption", "cryptsetup luksFormat", Category::Note, &["disk".into()]).unwrap();
        db.add_entry("tmux panes", "split-window -h", Category::Command, &["tmux".into()]).unwrap();
    }

    #[test]
    fn fts_prefix_search() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        assert_eq!(app.all_entries.len(), 3);

        app.search = "kerb".into(); // prefix of "kerberoast"
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.selected_entry().unwrap().title, "kerberoast attack");

        app.search.clear();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 3);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn delete_then_undo_restores() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        let before = app.all_entries.len();
        app.selected = 0;
        let victim = app.selected_entry().unwrap().title.clone();

        app.delete_selected().unwrap();
        assert_eq!(app.all_entries.len(), before - 1);
        assert!(!app.all_entries.iter().any(|e| e.title == victim));

        app.undo().unwrap();
        assert_eq!(app.all_entries.len(), before);
        assert!(app.all_entries.iter().any(|e| e.title == victim));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn n_cycles_matches_in_full_list() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();

        // search sets match_ids, then clear the filter but keep them (:noh style)
        app.search = "tmux".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1);
        app.search.clear();
        app.apply_filter(); // full list shown, match_ids retained
        assert_eq!(app.filtered.len(), 3);

        app.selected = 0;
        app.next_match(); // should jump to the tmux entry
        assert_eq!(app.selected_entry().unwrap().title, "tmux panes");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn like_fallback_for_punctuation() {
        let (db, path) = tmp_db();
        db.add_entry("port scan", "nmap -p-", Category::Command, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.search = "-p-".into(); // no usable FTS tokens -> LIKE fallback
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn edit_then_undo_reverts_fields_and_timestamps() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        let before = app.selected_entry().unwrap().clone();

        app.edit_form();
        app.form.title = "totally different title".into();
        app.form.content = "changed body".into();
        assert!(app.form.is_dirty());
        assert!(app.persist_form().unwrap());
        assert!(!app.form.is_dirty(), "saving clears the dirty flag");

        let edited = app.all_entries.iter().find(|e| e.id == before.id).unwrap();
        assert_eq!(edited.title, "totally different title");

        app.undo().unwrap();
        let back = app.all_entries.iter().find(|e| e.id == before.id).unwrap();
        assert_eq!(back.title, before.title);
        assert_eq!(back.content, before.content);
        assert_eq!(back.updated_at, before.updated_at, "original timestamp restored");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn undo_of_edit_recreates_row_if_deleted_after() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        let orig_title = app.selected_entry().unwrap().title.clone();

        app.edit_form();
        app.form.title = "edited".into();
        app.persist_form().unwrap();
        app.cancel_form();

        // now delete the edited row, then undo twice
        let pos = app.filtered.iter().position(|&i| app.all_entries[i].title == "edited").unwrap();
        app.selected = pos;
        app.delete_selected().unwrap();
        assert!(!app.all_entries.iter().any(|e| e.title == "edited"));

        app.undo().unwrap(); // undoes the delete
        assert!(app.all_entries.iter().any(|e| e.title == "edited"));
        app.undo().unwrap(); // undoes the edit
        assert!(app.all_entries.iter().any(|e| e.title == orig_title));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dirty_form_blocks_quit_until_forced() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.edit_form();
        app.form.content.push_str(" extra");

        assert!(!app.try_cancel_form(false), ":q refuses on unsaved changes");
        assert_eq!(app.screen, Screen::Form);
        assert!(app.try_cancel_form(true), ":q! discards");
        assert_eq!(app.screen, Screen::List);

        // an untouched form closes without complaint
        app.edit_form();
        assert!(app.try_cancel_form(false));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fts_index_stays_in_sync_across_edit_and_undo() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.selected = 0;

        // edit: old term must stop matching, new term must start matching
        app.edit_form();
        app.form.title = "zzunique marker".into();
        app.form.content = "zzunique body".into();
        app.persist_form().unwrap();
        app.cancel_form();

        app.search = "zzunique".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1, "edited text is searchable");

        app.search = "kerberoast".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 0, "old text no longer matches");

        // undo the edit: index must swing back
        app.search.clear();
        app.apply_filter();
        app.undo().unwrap();

        app.search = "zzunique".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 0, "reverted text is de-indexed");

        app.search = "kerberoast".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1, "original text searchable again");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn gt_cycles_tabs_forward_and_back_with_wrap() {
        let (db, path) = tmp_db();
        seed(&db); // 2 commands, 1 note, 0 tools
        let mut app = App::new(db).unwrap();
        assert_eq!(app.cat_filter, None);
        assert_eq!(app.filtered.len(), 3);

        app.next_tab(); // CMD
        assert_eq!(app.cat_filter, Some(Category::Command));
        assert_eq!(app.filtered.len(), 2);

        app.next_tab(); // NOTE
        assert_eq!(app.cat_filter, Some(Category::Note));
        assert_eq!(app.filtered.len(), 1);

        app.next_tab(); // TOOL (empty)
        assert_eq!(app.cat_filter, Some(Category::Tool));
        assert_eq!(app.filtered.len(), 0);

        app.next_tab(); // FAV (none favorited yet)
        assert_eq!(app.cat_filter, None);
        assert!(app.fav_filter);
        assert_eq!(app.filtered.len(), 0);

        app.next_tab(); // wraps back to ALL
        assert_eq!(app.cat_filter, None);
        assert!(!app.fav_filter);
        assert_eq!(app.filtered.len(), 3);

        app.prev_tab(); // wraps backwards to FAV
        assert!(app.fav_filter);
        assert_eq!(app.cat_filter, None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn tab_filter_composes_with_active_search() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();

        // "luks disk encryption" is a Note; searching finds it under ALL
        app.search = "luks".into();
        app.apply_filter();
        assert_eq!(app.filtered.len(), 1);

        app.next_tab(); // CMD — the note is filtered out, search still active
        assert_eq!(app.cat_filter, Some(Category::Command));
        assert_eq!(app.filtered.len(), 0);

        app.next_tab(); // NOTE — it comes back
        assert_eq!(app.filtered.len(), 1);
        assert_eq!(app.selected_entry().unwrap().title, "luks disk encryption");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn arrow_hint_sets_message_without_moving() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.selected = 1;
        app.arrow_hint();
        assert_eq!(app.selected, 1, "arrows must not move the cursor");
        assert!(app.status.as_deref().unwrap().contains("h j k l"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn redo_replays_delete_edit_and_favorite() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        let n0 = app.all_entries.len();

        // delete → undo → redo
        app.selected = 0;
        let title = app.selected_entry().unwrap().title.clone();
        app.delete_selected().unwrap();
        assert_eq!(app.all_entries.len(), n0 - 1);
        app.undo().unwrap();
        assert_eq!(app.all_entries.len(), n0);
        app.redo().unwrap();
        assert_eq!(app.all_entries.len(), n0 - 1);
        assert!(!app.all_entries.iter().any(|e| e.title == title));

        // favorite → undo → redo
        app.selected = 0;
        let id = app.selected_entry().unwrap().id;
        let was = app.selected_entry().unwrap().favorite;
        app.toggle_favorite_selected().unwrap();
        let fav_of = |a: &App| a.all_entries.iter().find(|e| e.id == id).unwrap().favorite;
        assert_eq!(fav_of(&app), !was);
        app.undo().unwrap();
        assert_eq!(fav_of(&app), was, "undo restores the favorite flag");
        app.redo().unwrap();
        assert_eq!(fav_of(&app), !was, "redo re-applies it");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn new_change_clears_the_redo_branch() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.delete_selected().unwrap();
        app.undo().unwrap();
        assert_eq!(app.redo_stack.len(), 1);

        app.selected = 0;
        app.toggle_favorite_selected().unwrap(); // a fresh change
        assert!(app.redo_stack.is_empty(), "redo branch is dropped after a new change");

        app.redo().unwrap();
        assert!(app.status.as_deref().unwrap().contains("newest"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn global_delete_removes_matches_and_undoes_as_one_step() {
        let (db, path) = tmp_db();
        // 3 entries mentioning nmap, 1 that does not
        db.add_entry("nmap basics", "nmap -sV", Category::Command, &[]).unwrap();
        db.add_entry("nmap scripts", "nmap --script vuln", Category::Command, &[]).unwrap();
        db.add_entry("nmap timing", "nmap -T4", Category::Note, &[]).unwrap();
        db.add_entry("tmux panes", "split-window", Category::Command, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        assert_eq!(app.all_entries.len(), 4);

        app.execute_command("g/nmap/d").unwrap();
        assert_eq!(app.all_entries.len(), 1, "all nmap entries deleted");
        assert_eq!(app.all_entries[0].title, "tmux panes");
        assert_eq!(app.undo_stack.len(), 1, "the batch is a single undo step");

        app.undo().unwrap();
        assert_eq!(app.all_entries.len(), 4, "one u brings all of them back");

        app.redo().unwrap();
        assert_eq!(app.all_entries.len(), 1, "one Ctrl-r removes them again");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn global_delete_respects_the_active_tab() {
        let (db, path) = tmp_db();
        db.add_entry("nmap basics", "nmap -sV", Category::Command, &[]).unwrap();
        db.add_entry("nmap timing", "nmap -T4", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();

        app.set_cat_filter(Some(Category::Note)); // :g acts on the visible buffer
        app.execute_command("g/nmap/d").unwrap();

        assert_eq!(app.all_entries.len(), 1);
        assert_eq!(app.all_entries[0].category, Category::Command, "CMD entry untouched");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn global_favorite_marks_matches() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();

        app.execute_command("g/tmux/fav").unwrap();
        let favs: Vec<_> = app.all_entries.iter().filter(|e| e.favorite).collect();
        assert_eq!(favs.len(), 1);
        assert_eq!(favs[0].title, "tmux panes");

        app.undo().unwrap();
        assert_eq!(app.all_entries.iter().filter(|e| e.favorite).count(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn theme_lookup_and_color_mode() {
        use crate::theme::Theme;
        assert!(Theme::by_name("gruvbox").is_some());
        assert!(Theme::by_name("GRUVBOX").is_some(), "lookup is case-insensitive");
        assert!(Theme::by_name("nope").is_none());
        // ANSI fallback and RGB resolve differently for the same shade
        let t = Theme::by_name("nord").unwrap();
        assert_ne!(t.with_color_mode(true).accent_c(), t.with_color_mode(false).accent_c());
        let _ = std::fs::remove_file("/dev/null");
    }

    #[test]
    fn safelight_is_the_default_theme() {
        use crate::theme::Theme;
        assert_eq!(Theme::by_name("safelight").unwrap().name, "safelight");
        assert!(Theme::names().starts_with("safelight"));
    }

    #[test]
    fn heading_levels_get_distinct_colors() {
        use crate::theme::SAFELIGHT;
        let th = SAFELIGHT.with_color_mode(true);
        let h1 = th.head_c(1);
        let h3 = th.head_c(3);
        assert_ne!(h1, h3, "h1 and h3 differ (amber vs cyan)");
        assert_eq!(th.head_c(9), th.head_c(6), "levels beyond 6 clamp");
        assert_eq!(th.head_c(0), th.head_c(1), "level 0 clamps up");
    }

    /// Render the real UI into a test backend and confirm themed colors reach
    /// the screen — the closest thing to eyeballing it without a TTY.
    #[test]
    fn renders_themed_colors_into_the_buffer() {
        use ratatui::{backend::TestBackend, style::Color, Terminal};
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.theme = crate::theme::SAFELIGHT.with_color_mode(true);

        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();

        let substrate = Color::Rgb(0x16, 0x12, 0x0F);
        let cyan      = Color::Rgb(0x62, 0xBE, 0xC4);

        let mut painted = 0usize;
        let mut accent  = 0usize;
        for y in 0..24u16 {
            for x in 0..100u16 {
                let cell = &buf[(x, y)];
                if cell.bg == substrate { painted += 1; }
                if cell.fg == cyan { accent += 1; }
            }
        }
        assert!(painted > 1000, "theme background is painted across the surface");
        assert!(accent > 0, "accent color is used (header/title)");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dedupe_exact_keeps_one_and_merges_tags() {
        let (db, path) = tmp_db();
        // same title+content twice, different tags, one favorited
        db.add_entry("git flow", "git flow init", Category::Command, &["git".into()]).unwrap();
        db.add_entry("git flow", "git flow init", Category::Command, &["vcs".into()]).unwrap();
        db.add_entry("unique one", "body", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        let dup_id = app.all_entries.iter().find(|e| e.title == "git flow").unwrap().id;
        app.db.set_favorite(dup_id, true).unwrap();
        app.reload().unwrap();
        assert_eq!(app.all_entries.len(), 3);

        // report only — nothing removed
        app.dedupe(DupKey::Exact, KeepRule::Longest, false).unwrap();
        assert_eq!(app.all_entries.len(), 3, "reporting must not delete");
        assert!(app.status.as_deref().unwrap().contains("removable"));

        app.dedupe(DupKey::Exact, KeepRule::Longest, true).unwrap();
        assert_eq!(app.all_entries.len(), 2, "one copy removed");
        let kept = app.all_entries.iter().find(|e| e.title == "git flow").unwrap();
        assert!(kept.tags.contains(&"git".to_string()));
        assert!(kept.tags.contains(&"vcs".to_string()), "tags merged from both copies");
        assert!(kept.favorite, "favorite survives the merge");

        app.undo().unwrap();
        assert_eq!(app.all_entries.len(), 3, "the whole dedupe is one undo step");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dedupe_title_key_keeps_the_longest_copy() {
        let (db, path) = tmp_db();
        db.add_entry("nmap", "short", Category::Command, &[]).unwrap();
        db.add_entry("nmap", "a much longer and more complete body", Category::Command, &[]).unwrap();
        let mut app = App::new(db).unwrap();

        // exact key does NOT match these (content differs)
        app.dedupe(DupKey::Exact, KeepRule::Longest, true).unwrap();
        assert_eq!(app.all_entries.len(), 2, "exact key leaves differing content alone");

        app.dedupe(DupKey::Title, KeepRule::Longest, true).unwrap();
        assert_eq!(app.all_entries.len(), 1);
        assert!(app.all_entries[0].content.starts_with("a much longer"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn normalized_key_catches_whitespace_only_differences() {
        let (db, path) = tmp_db();
        db.add_entry("tmux", "split-window   -h\n\n", Category::Command, &[]).unwrap();
        db.add_entry("tmux", "split-window -h", Category::Command, &[]).unwrap();
        let mut app = App::new(db).unwrap();

        app.dedupe(DupKey::Exact, KeepRule::First, true).unwrap();
        assert_eq!(app.all_entries.len(), 2, "exact sees them as different");

        app.dedupe(DupKey::Normalized, KeepRule::First, true).unwrap();
        assert_eq!(app.all_entries.len(), 1, "normalized collapses the whitespace difference");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dedupe_is_idempotent_and_noop_on_clean_data() {
        let (db, path) = tmp_db();
        seed(&db);
        let mut app = App::new(db).unwrap();
        app.dedupe(DupKey::Exact, KeepRule::Longest, true).unwrap();
        assert_eq!(app.all_entries.len(), 3);
        assert!(app.status.as_deref().unwrap().contains("No duplicates"));
        assert!(app.undo_stack.is_empty(), "a no-op records no undo step");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn gt_visits_the_favorites_tab() {
        let (db, path) = tmp_db();
        seed(&db); // 3 entries, none favorited
        let mut app = App::new(db).unwrap();
        let id = app.all_entries[0].id;
        app.db.set_favorite(id, true).unwrap();
        app.reload().unwrap();

        for _ in 0..4 {
            app.next_tab(); // CMD, NOTE, TOOL, FAV
        }
        assert!(app.fav_filter, "5th tab is Favorites");
        assert_eq!(app.cat_filter, None, "Favorites tab is not restricted to a category");
        assert_eq!(app.filtered.len(), 1);
        assert!(app.filtered.iter().all(|&i| app.all_entries[i].favorite));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn visual_line_yank_selects_exact_lines_no_extra() {
        let (db, path) = tmp_db();
        db.add_entry("multi", "one\ntwo\nthree\nfour", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.open_view();

        app.enter_visual_line();       // anchor at line 0 ("one")
        app.view_down(2);              // cursor now at line 2 ("three")
        let text = app.visual_selection_text();
        assert_eq!(text, "one\ntwo\nthree", "exactly the spanned lines, nothing appended");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn visual_line_yank_works_regardless_of_direction() {
        let (db, path) = tmp_db();
        db.add_entry("multi", "one\ntwo\nthree", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.open_view();

        app.view_down(2);              // cursor at line 2 ("three")
        app.enter_visual_line();       // anchor at line 2
        app.view_up(2);                // cursor back to line 0 ("one")
        let text = app.visual_selection_text();
        assert_eq!(text, "one\ntwo\nthree", "selecting upward still yields ordered lines");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn visual_charwise_yank_selects_exact_span() {
        let (db, path) = tmp_db();
        db.add_entry("multi", "hello world\nsecond line", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.open_view();

        // select "world" (columns 6..=10 on line 0)
        for _ in 0..6 {
            app.cursor_right();
        }
        app.enter_visual();
        for _ in 0..4 {
            app.cursor_right();
        }
        let text = app.visual_selection_text();
        assert_eq!(text, "world", "charwise visual copies only the highlighted characters");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn yank_current_line_targets_only_the_cursor_line_not_the_whole_entry() {
        let (db, path) = tmp_db();
        db.add_entry("multi", "one\ntwo\nthree", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.open_view();

        app.view_down(1); // cursor now on "two"
        assert_eq!(
            app.content_lines().get(app.cursor_line).map(String::as_str),
            Some("two"),
            "yank_current_line copies whatever line the cursor sits on, not the full entry"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn visual_charwise_yank_spans_multiple_lines_cleanly() {
        let (db, path) = tmp_db();
        db.add_entry("multi", "abc\ndef\nghi", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.open_view();

        app.cursor_right(); // col 1 on "abc" -> 'b'
        app.enter_visual();
        app.view_down(2);   // line 2 "ghi"; column carries over -> still col 1 ('h')
        let text = app.visual_selection_text();
        assert_eq!(text, "bc\ndef\ngh", "first line from anchor, middle line whole, last line up to cursor");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cursor_down_clamps_at_last_line_and_autoscrolls() {
        let (db, path) = tmp_db();
        let body = (0..30).map(|i| format!("line{}", i)).collect::<Vec<_>>().join("\n");
        db.add_entry("long", &body, Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;
        app.open_view();
        app.view_height = 10;

        app.view_down(100); // way past the end
        assert_eq!(app.cursor_line, 29, "clamped to the last line");
        assert!(app.view_scroll > 0, "scrolled to keep the cursor visible");

        app.view_top();
        assert_eq!(app.cursor_line, 0);
        assert_eq!(app.view_scroll, 0);
        let _ = std::fs::remove_file(path);
    }

    /// Renders the full view in Visual-Line mode and confirms the selection
    /// is actually painted — a regression guard for the tint-line rendering.
    #[test]
    fn visual_line_selection_is_painted_in_the_full_view() {
        use ratatui::{backend::TestBackend, Terminal};
        let (db, path) = tmp_db();
        db.add_entry("multi", "one\ntwo\nthree\nfour", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.theme = crate::theme::SAFELIGHT.with_color_mode(true);
        app.selected = 0;
        app.open_view();
        app.enter_visual_line();
        app.view_down(1); // select lines 0-1 ("one", "two")

        let mut term = Terminal::new(TestBackend::new(80, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();

        let sel = app.theme.sel_c();
        let painted = (0..80u16)
            .flat_map(|x| (0..24u16).map(move |y| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].bg == sel)
            .count();
        assert!(painted > 0, "the visual-line selection must be visibly tinted");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn visual_line_selection_is_painted_in_the_list_preview() {
        use ratatui::{backend::TestBackend, Terminal};
        let (db, path) = tmp_db();
        db.add_entry("multi", "one\ntwo\nthree\nfour", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.theme = crate::theme::SAFELIGHT.with_color_mode(true);
        app.selected = 0;
        assert_eq!(app.screen, Screen::List, "selection must work from the list's side preview too");

        app.enter_visual_line();
        app.view_down(1); // select lines 0-1 ("one", "two")
        assert_eq!(app.visual_selection_text(), "one\ntwo");

        let mut term = Terminal::new(TestBackend::new(100, 24)).unwrap();
        term.draw(|f| crate::ui::render(f, &mut app)).unwrap();
        let buf = term.backend().buffer().clone();

        let sel = app.theme.sel_c();
        let painted = (0..100u16)
            .flat_map(|x| (0..24u16).map(move |y| (x, y)))
            .filter(|&(x, y)| buf[(x, y)].bg == sel)
            .count();
        assert!(painted > 0, "the side preview must visibly tint the selection too");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn list_preview_visual_resets_cursor_each_time_it_is_entered() {
        let (db, path) = tmp_db();
        db.add_entry("multi", "one\ntwo\nthree\nfour\nfive", Category::Note, &[]).unwrap();
        let mut app = App::new(db).unwrap();
        app.selected = 0;

        app.enter_visual_line();
        app.view_down(3); // cursor now at line 3
        // `yank_visual` would exit Visual mode the same way (it always calls
        // `exit_visual` at the end) — call it directly here to avoid shelling
        // out to a real clipboard tool in a test.
        app.exit_visual(); // cursor/scroll left at line 3

        // Re-entering fresh (as the 'v'/'V' key handlers do via view_top())
        // must start at the top again, not resume from the stale position.
        app.view_top();
        app.enter_visual_line();
        assert_eq!(app.visual_selection_text(), "one", "fresh selection starts at line 0");
        let _ = std::fs::remove_file(path);
    }
}
