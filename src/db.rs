use anyhow::{bail, Result};
use rusqlite::{params, params_from_iter, Connection, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::derive::derive_tool;
use crate::models::{content_hash, Category, DeletedEntry, Entry, NewEntry, Source};
use crate::query::{osa_distance, typo_budget, Query};
use crate::util::{like_escape, now_str, squeeze};

/// Current on-disk schema, stored in `PRAGMA user_version`.
pub const SCHEMA_VERSION: i64 = 2;

/// A run of at least this many rows created within a second of each other is a
/// bulk import (the importer stamps rows one second apart).
const MIN_BATCH: usize = 20;

pub struct Database {
    conn: Connection,
    pub path: PathBuf,
    /// Set when opening the database changed it in a way worth telling the user.
    pub migration_note: Option<String>,
}

// ─── schema ──────────────────────────────────────────────────────────────────

const CREATE_ENTRIES: &str = "
CREATE TABLE IF NOT EXISTS entries (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    title       TEXT    NOT NULL,
    content     TEXT    NOT NULL DEFAULT '',
    category    TEXT    NOT NULL DEFAULT 'note',
    tags        TEXT    NOT NULL DEFAULT '',
    favorite    INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT    NOT NULL,
    updated_at  TEXT    NOT NULL,
    tool        TEXT    NOT NULL DEFAULT '',
    command     TEXT    NOT NULL DEFAULT '',
    keywords    TEXT    NOT NULL DEFAULT '',
    danger      INTEGER NOT NULL DEFAULT 0,
    uses        INTEGER NOT NULL DEFAULT 0,
    last_used   TEXT    NOT NULL DEFAULT '',
    source      TEXT    NOT NULL DEFAULT 'user',
    pack_key    TEXT    NOT NULL DEFAULT '',
    pack_hash   TEXT    NOT NULL DEFAULT ''
);";

/// Columns added after the original release, with their DDL.
const NEW_COLUMNS: &[(&str, &str)] = &[
    ("favorite",  "INTEGER NOT NULL DEFAULT 0"),
    ("tool",      "TEXT NOT NULL DEFAULT ''"),
    ("command",   "TEXT NOT NULL DEFAULT ''"),
    ("keywords",  "TEXT NOT NULL DEFAULT ''"),
    ("danger",    "INTEGER NOT NULL DEFAULT 0"),
    ("uses",      "INTEGER NOT NULL DEFAULT 0"),
    ("last_used", "TEXT NOT NULL DEFAULT ''"),
    ("source",    "TEXT NOT NULL DEFAULT 'user'"),
    ("pack_key",  "TEXT NOT NULL DEFAULT ''"),
    ("pack_hash", "TEXT NOT NULL DEFAULT ''"),
];

const CREATE_AUX: &str = "
CREATE TABLE IF NOT EXISTS vars (
    name       TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    pinned     INTEGER NOT NULL DEFAULT 0,
    updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pack_tombstones (
    key TEXT PRIMARY KEY
);";

const CREATE_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS idx_entries_tool ON entries(tool);
CREATE INDEX IF NOT EXISTS idx_entries_pack_key ON entries(pack_key) WHERE pack_key <> '';";

const DROP_FTS: &str = "
DROP TRIGGER IF EXISTS entries_ai;
DROP TRIGGER IF EXISTS entries_ad;
DROP TRIGGER IF EXISTS entries_au;
DROP TABLE IF EXISTS entries_vocab;
DROP TABLE IF EXISTS entries_fts;";

const CREATE_FTS: &str = "
CREATE VIRTUAL TABLE IF NOT EXISTS entries_fts USING fts5(
    title, tool, tags, keywords, command, content,
    content='entries', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2',
    prefix='2 3 4'
);
CREATE VIRTUAL TABLE IF NOT EXISTS entries_vocab USING fts5vocab(entries_fts, 'row');

CREATE TRIGGER IF NOT EXISTS entries_ai AFTER INSERT ON entries BEGIN
    INSERT INTO entries_fts(rowid, title, tool, tags, keywords, command, content)
    VALUES (new.id, new.title, new.tool, new.tags, new.keywords, new.command, new.content);
    DELETE FROM pack_tombstones WHERE key = new.pack_key AND new.pack_key <> '';
END;

CREATE TRIGGER IF NOT EXISTS entries_ad AFTER DELETE ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, tool, tags, keywords, command, content)
    VALUES ('delete', old.id, old.title, old.tool, old.tags, old.keywords, old.command, old.content);
    INSERT OR IGNORE INTO pack_tombstones(key) SELECT old.pack_key WHERE old.pack_key <> '';
END;

CREATE TRIGGER IF NOT EXISTS entries_au
    AFTER UPDATE OF title, tool, tags, keywords, command, content ON entries BEGIN
    INSERT INTO entries_fts(entries_fts, rowid, title, tool, tags, keywords, command, content)
    VALUES ('delete', old.id, old.title, old.tool, old.tags, old.keywords, old.command, old.content);
    INSERT INTO entries_fts(rowid, title, tool, tags, keywords, command, content)
    VALUES (new.id, new.title, new.tool, new.tags, new.keywords, new.command, new.content);
END;";

/// Column list for SELECTs that map through [`row_to_entry`].
fn cols(prefix: &str) -> String {
    [
        "id", "title", "content", "category", "tags", "favorite", "created_at", "updated_at", "tool",
        "command", "keywords", "danger", "uses", "last_used", "source", "pack_key", "pack_hash",
    ]
    .iter()
    .map(|c| format!("{}{}", prefix, c))
    .collect::<Vec<_>>()
    .join(", ")
}

fn row_to_entry(row: &Row) -> rusqlite::Result<Entry> {
    let tags_str: String = row.get(4)?;
    Ok(Entry {
        id:         row.get(0)?,
        title:      row.get(1)?,
        content:    row.get(2)?,
        category:   Category::from_str(&row.get::<_, String>(3)?),
        tags:       parse_tags(&tags_str),
        favorite:   row.get::<_, i64>(5)? != 0,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        tool:       row.get(8)?,
        command:    row.get(9)?,
        keywords:   row.get(10)?,
        danger:     row.get::<_, i64>(11)? != 0,
        uses:       row.get(12)?,
        last_used:  row.get(13)?,
        source:     Source::from_db(&row.get::<_, String>(14)?),
        pack_key:   row.get(15)?,
        pack_hash:  row.get(16)?,
    })
}

fn parse_tags(s: &str) -> Vec<String> {
    if s.is_empty() {
        vec![]
    } else {
        s.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect()
    }
}

// ─── search types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub id: i64,
    /// Lower is better (bm25 convention), after usage/source adjustments.
    pub score: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    pub hits: Vec<Hit>,
    /// Set when the query had to be relaxed (typo corrected, partial match…).
    pub note: Option<String>,
}

/// bm25 column weights, in FTS column order: title, tool, tags, keywords, command, content.
const BM25: &str = "bm25(entries_fts, 10.0, 8.0, 5.0, 6.0, 4.0, 1.0)";

/// Every searchable text column, for substring matching.
const HAYSTACK: &str =
    "(e.title || ' ' || e.tool || ' ' || e.tags || ' ' || e.keywords || ' ' || e.command || ' ' || e.content)";

fn adjust(score: f64, uses: i64, favorite: bool, source: Source) -> f64 {
    // bm25 is negative-is-better, so multiplying the magnitude improves a hit.
    let boost = 1.0 + 0.12 * ((1 + uses.max(0)) as f64).ln() + if favorite { 0.25 } else { 0.0 };
    score * boost * source.prior()
}

/// SQL fragment + params for the structured filters of a query.
fn filter_sql(q: &Query) -> (String, Vec<String>) {
    let mut sql = String::new();
    let mut p: Vec<String> = Vec::new();
    if let Some(t) = &q.tool {
        sql.push_str(" AND (e.tool = ? OR e.tool LIKE ? ESCAPE '\\')");
        p.push(t.clone());
        p.push(format!("{}%", like_escape(t)));
    }
    for tag in &q.tags {
        sql.push_str(" AND (',' || lower(e.tags) || ',') LIKE ? ESCAPE '\\'");
        p.push(format!("%,{},%", like_escape(tag)));
    }
    if let Some(c) = &q.category {
        sql.push_str(" AND e.category = ?");
        p.push(c.as_str().to_string());
    }
    if q.favorite {
        sql.push_str(" AND e.favorite = 1");
    }
    if q.danger {
        sql.push_str(" AND e.danger = 1");
    }
    if let Some(s) = q.source {
        sql.push_str(" AND e.source = ?");
        p.push(s.as_str().to_string());
    }
    (sql, p)
}

// ─── stats / health types ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub total: i64,
    pub by_category: Vec<(String, i64)>,
    pub by_source: Vec<(String, i64)>,
    pub favorites: i64,
    pub with_command: i64,
    pub dangerous: i64,
    pub total_uses: i64,
    pub distinct_tools: i64,
    pub untooled: i64,
    pub vars: i64,
    pub db_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SyncStats {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    /// Pack entries left alone because you edited them.
    pub kept: usize,
    /// Pack entries you deleted, so they were not re-added.
    pub skipped_deleted: usize,
    pub unchanged: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Var {
    pub name: String,
    pub value: String,
    pub pinned: bool,
}

// ─── Database ────────────────────────────────────────────────────────────────

impl Database {
    pub fn new(path: &Path) -> Result<Self> {
        let existed = path.metadata().map(|m| m.len() > 0).unwrap_or(false);
        let conn = Connection::open(path)?;
        // A couple of pragmas that make bulk import and reads snappier.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;",
        )
        .ok();
        let mut db = Database { conn, path: path.to_path_buf(), migration_note: None };
        db.init(existed)?;
        Ok(db)
    }

    fn table_exists(&self, name: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
            params![name],
            |r| Ok(r.get::<_, i64>(0)? > 0),
        )?)
    }

    fn init(&mut self, _existed: bool) -> Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version > SCHEMA_VERSION {
            bail!(
                "this database uses schema v{} but this recall only understands up to v{} — upgrade recall",
                version,
                SCHEMA_VERSION
            );
        }
        let has_entries = self.table_exists("entries")?;

        if !has_entries {
            self.create_fresh()?;
        } else if version < SCHEMA_VERSION {
            let n: i64 = self.conn.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?;
            if n > 0 {
                let bak = self.backup_for_migration()?;
                self.migration_note = Some(format!(
                    "recall: upgraded your database to schema v{} (backup: {})",
                    SCHEMA_VERSION,
                    bak.display()
                ));
            }
            self.migrate_legacy()?;
        } else {
            self.ensure_objects()?;
        }
        Ok(())
    }

    fn create_fresh(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(CREATE_ENTRIES)?;
        tx.execute_batch(CREATE_AUX)?;
        tx.execute_batch(CREATE_INDEXES)?;
        tx.execute_batch(DROP_FTS)?;
        tx.execute_batch(CREATE_FTS)?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION))?;
        tx.commit()?;
        Ok(())
    }

    /// Idempotent repair for a current-version database: recreate anything that
    /// went missing (e.g. a table dropped by hand).
    fn ensure_objects(&mut self) -> Result<()> {
        self.conn.execute_batch(CREATE_AUX)?;
        self.conn.execute_batch(CREATE_INDEXES)?;
        if !self.table_exists("entries_fts")? {
            self.conn.execute_batch(CREATE_FTS)?;
            self.conn.execute("INSERT INTO entries_fts(entries_fts) VALUES('rebuild')", [])?;
        } else if !self.table_exists("entries_vocab")? {
            self.conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS entries_vocab USING fts5vocab(entries_fts, 'row');",
            )?;
        }
        Ok(())
    }

    /// Bring a v0/v1 database (the original schema) up to v2 in one transaction.
    /// Everything is additive: no existing column or row is rewritten except the
    /// new `tool` / `source` columns, which are derived.
    fn migrate_legacy(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute_batch(DROP_FTS)?;

        let present: HashSet<String> = {
            let mut stmt = tx.prepare("PRAGMA table_info(entries)")?;
            let names = stmt
                .query_map([], |r| r.get::<_, String>(1))?
                .collect::<rusqlite::Result<HashSet<_>>>()?;
            names
        };
        for (name, ddl) in NEW_COLUMNS {
            if !present.contains(*name) {
                tx.execute(&format!("ALTER TABLE entries ADD COLUMN {} {}", name, ddl), [])?;
            }
        }
        tx.execute_batch(CREATE_AUX)?;

        // Derive `tool`, and tell bulk-imported rows from hand-made ones.
        let rows: Vec<(i64, String, String, String, String)> = {
            let mut stmt = tx.prepare("SELECT id, title, content, tags, created_at FROM entries")?;
            let v = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            v
        };
        let stamps: Vec<(i64, String)> = rows.iter().map(|r| (r.0, r.4.clone())).collect();
        let imported = detect_import_batches(&stamps);
        {
            let mut upd = tx.prepare("UPDATE entries SET tool = ?1, source = ?2 WHERE id = ?3")?;
            for (id, title, content, tags, _) in &rows {
                let tool = derive_tool(title, content, &parse_tags(tags));
                let src = if imported.contains(id) { Source::Import } else { Source::User };
                upd.execute(params![tool, src.as_str(), id])?;
            }
        }

        tx.execute_batch(CREATE_INDEXES)?;
        tx.execute_batch(CREATE_FTS)?;
        tx.execute("INSERT INTO entries_fts(entries_fts) VALUES('rebuild')", [])?;
        tx.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION))?;
        tx.commit()?;
        Ok(())
    }

    fn backup_for_migration(&self) -> Result<PathBuf> {
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let mut name = self.path.as_os_str().to_owned();
        name.push(format!(".pre-v{}-{}.bak", SCHEMA_VERSION, stamp));
        let dest = PathBuf::from(name);
        self.backup_to(&dest)?;
        Ok(dest)
    }

    /// Write a consistent, compacted copy of the database (safe while in WAL mode).
    pub fn backup_to(&self, dest: &Path) -> Result<()> {
        if dest.exists() {
            bail!("{} already exists — refusing to overwrite", dest.display());
        }
        let lit = dest.to_string_lossy().replace('\'', "''");
        self.conn.execute_batch(&format!("VACUUM INTO '{}'", lit))?;
        Ok(())
    }

    // ── writes ───────────────────────────────────────────────────────────────

    /// Bulk insert in one transaction. Triggers keep the FTS index in sync.
    /// Timestamps descend by position so the batch preserves source order under
    /// `ORDER BY updated_at DESC`.
    pub fn add_entries(&mut self, items: &[NewEntry]) -> Result<usize> {
        let base = chrono::Local::now();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO entries
                   (title, content, category, tags, created_at, updated_at,
                    tool, command, keywords, danger, source, pack_key, pack_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            )?;
            for (i, e) in items.iter().enumerate() {
                let ts = (base - chrono::Duration::seconds(i as i64))
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string();
                let hash = if e.pack_key.is_empty() { String::new() } else { e.content_hash() };
                stmt.execute(params![
                    e.title,
                    e.content,
                    e.category.as_str(),
                    e.tags.join(","),
                    ts,
                    e.tool,
                    e.command,
                    e.keywords,
                    e.danger as i64,
                    e.source.as_str(),
                    e.pack_key,
                    hash,
                ])?;
            }
        }
        tx.commit()?;
        Ok(items.len())
    }

    /// (title, content) pairs already stored — used by the importer to skip
    /// duplicates so re-importing a file is idempotent.
    pub fn existing_title_content(&self) -> Result<HashSet<(String, String)>> {
        let mut stmt = self.conn.prepare("SELECT title, content FROM entries")?;
        let set = stmt
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(set)
    }

    /// `tool \x1f normalized-command` for every entry that has a command, so
    /// the ingest pipelines can skip what is already known.
    pub fn existing_command_keys(&self) -> Result<HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT tool, command FROM entries WHERE command <> ''")?;
        let set = stmt
            .query_map([], |row| {
                let tool: String = row.get(0)?;
                let cmd: String = row.get(1)?;
                Ok(command_key(&tool, &cmd))
            })?
            .filter_map(|r| r.ok())
            .collect();
        Ok(set)
    }

    /// Simple constructor kept for tests and casual callers; `add_new` is the
    /// full-fidelity entry point everything else (the CLI, the TUI form,
    /// import/ingest/pack) goes through.
    #[allow(dead_code)]
    pub fn add_entry(&self, title: &str, content: &str, category: Category, tags: &[String]) -> Result<i64> {
        self.add_new(&NewEntry {
            title: title.to_string(),
            content: content.to_string(),
            category,
            tags: tags.to_vec(),
            ..Default::default()
        })
    }

    /// Insert one fully-specified entry stamped "now".
    pub fn add_new(&self, e: &NewEntry) -> Result<i64> {
        let now = now_str();
        let hash = if e.pack_key.is_empty() { String::new() } else { e.content_hash() };
        self.conn.execute(
            "INSERT INTO entries
               (title, content, category, tags, created_at, updated_at,
                tool, command, keywords, danger, source, pack_key, pack_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                e.title,
                e.content,
                e.category.as_str(),
                e.tags.join(","),
                now,
                e.tool,
                e.command,
                e.keywords,
                e.danger as i64,
                e.source.as_str(),
                e.pack_key,
                hash,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn update_entry(
        &self,
        id: i64,
        title: &str,
        content: &str,
        category: Category,
        tags: &[String],
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE entries
             SET title=?1, content=?2, category=?3, tags=?4, updated_at=?5
             WHERE id=?6",
            params![title, content, category.as_str(), tags.join(","), now_str(), id],
        )?;
        Ok(())
    }

    /// Save every user-editable field (the TUI form).
    pub fn update_full(&self, id: i64, e: &NewEntry) -> Result<()> {
        self.conn.execute(
            "UPDATE entries
             SET title=?1, content=?2, category=?3, tags=?4, tool=?5, command=?6,
                 keywords=?7, danger=?8, updated_at=?9
             WHERE id=?10",
            params![
                e.title,
                e.content,
                e.category.as_str(),
                e.tags.join(","),
                e.tool,
                e.command,
                e.keywords,
                e.danger as i64,
                now_str(),
                id
            ],
        )?;
        Ok(())
    }

    /// Re-insert a previously deleted entry, preserving its timestamps, usage
    /// and favorite flag. Returns the new row id.
    pub fn restore_entry(&self, e: &DeletedEntry) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO entries
               (title, content, category, tags, favorite, created_at, updated_at,
                tool, command, keywords, danger, uses, last_used, source, pack_key, pack_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                e.title,
                e.content,
                e.category.as_str(),
                e.tags.join(","),
                e.favorite as i64,
                e.created_at,
                e.updated_at,
                e.tool,
                e.command,
                e.keywords,
                e.danger as i64,
                e.uses,
                e.last_used,
                e.source.as_str(),
                e.pack_key,
                e.pack_hash,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Write a snapshot back onto an existing row (undo of an edit). Restores
    /// the original timestamps too, so an undone edit leaves no trace. Returns
    /// the number of rows affected — 0 means the row is gone.
    pub fn restore_edit(&self, id: i64, e: &DeletedEntry) -> Result<usize> {
        let n = self.conn.execute(
            "UPDATE entries
             SET title=?1, content=?2, category=?3, tags=?4, favorite=?5,
                 created_at=?6, updated_at=?7, tool=?8, command=?9, keywords=?10,
                 danger=?11, uses=?12, last_used=?13, source=?14, pack_key=?15, pack_hash=?16
             WHERE id=?17",
            params![
                e.title,
                e.content,
                e.category.as_str(),
                e.tags.join(","),
                e.favorite as i64,
                e.created_at,
                e.updated_at,
                e.tool,
                e.command,
                e.keywords,
                e.danger as i64,
                e.uses,
                e.last_used,
                e.source.as_str(),
                e.pack_key,
                e.pack_hash,
                id
            ],
        )?;
        Ok(n)
    }

    pub fn set_favorite(&self, id: i64, favorite: bool) -> Result<()> {
        self.conn
            .execute("UPDATE entries SET favorite=?1 WHERE id=?2", params![favorite as i64, id])?;
        Ok(())
    }

    pub fn delete_entry(&self, id: i64) -> Result<()> {
        self.conn.execute("DELETE FROM entries WHERE id=?1", params![id])?;
        Ok(())
    }

    /// Count a use: bumps `uses` and stamps `last_used`, which feed ranking.
    pub fn record_use(&self, id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE entries SET uses = uses + 1, last_used = ?1 WHERE id = ?2",
            params![now_str(), id],
        )?;
        Ok(())
    }

    /// Apply a dedupe plan in one transaction: update survivors with merged
    /// tags/favorite, then delete the duplicate rows. Returns (removed, merged).
    pub fn apply_dedupe(&mut self, plan: &[crate::dedupe::Group]) -> Result<(usize, usize)> {
        let now = now_str();
        let mut removed = 0usize;
        let mut merged = 0usize;
        let tx = self.conn.transaction()?;
        {
            let mut upd = tx.prepare("UPDATE entries SET tags=?1, favorite=?2, updated_at=?3 WHERE id=?4")?;
            let mut del = tx.prepare("DELETE FROM entries WHERE id=?1")?;
            for g in plan {
                if g.survivor_changed {
                    upd.execute(params![g.merged_tags.join(","), g.favorite as i64, now, g.survivor])?;
                    merged += 1;
                }
                for v in &g.victims {
                    del.execute(params![v])?;
                    removed += 1;
                }
            }
        }
        tx.commit()?;
        Ok((removed, merged))
    }

    // ── reads ────────────────────────────────────────────────────────────────

    /// Fetch a single entry by id.
    pub fn get_entry(&self, id: i64) -> Result<Option<Entry>> {
        let sql = format!("SELECT {} FROM entries WHERE id=?1", cols(""));
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![id], row_to_entry)?;
        Ok(match rows.next() {
            Some(r) => Some(r?),
            None => None,
        })
    }

    /// Entries for these ids, in the order given (missing ids are skipped).
    pub fn get_entries(&self, ids: &[i64]) -> Result<Vec<Entry>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(e) = self.get_entry(*id)? {
                out.push(e);
            }
        }
        Ok(out)
    }

    pub fn get_all_entries(&self) -> Result<Vec<Entry>> {
        let sql = format!("SELECT {} FROM entries ORDER BY id", cols(""));
        let mut stmt = self.conn.prepare(&sql)?;
        let entries = stmt.query_map([], row_to_entry)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(entries)
    }

    pub fn count(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT count(*) FROM entries", [], |r| r.get(0))?)
    }

    // ── search ───────────────────────────────────────────────────────────────

    /// Ranked ids for a raw query string (the TUI's live filter).
    pub fn search_ids(&self, query: &str) -> Result<Vec<i64>> {
        let q = Query::parse(query);
        Ok(self.search(&q, 0)?.hits.into_iter().map(|h| h.id).collect())
    }

    /// Whole entries for a raw query, best first (0 = no limit).
    pub fn search_entries(&self, query: &str, limit: usize) -> Result<Vec<Entry>> {
        let q = Query::parse(query);
        let ids: Vec<i64> = self.search(&q, limit)?.hits.into_iter().map(|h| h.id).collect();
        self.get_entries(&ids)
    }

    /// The search engine. Order of attempts, stopping at the first that finds
    /// something: every word (FTS prefix AND) → typo-corrected words → any word
    /// (OR) → plain substring scan. `limit == 0` means unlimited.
    pub fn search(&self, q: &Query, limit: usize) -> Result<SearchOutcome> {
        let (mut fsql, mut fparams) = filter_sql(q);
        // Flag-shaped words (-sV, --script) are exact substring requirements.
        for f in q.flag_terms() {
            fsql.push_str(&format!(" AND {} LIKE ? ESCAPE '\\'", HAYSTACK));
            fparams.push(format!("%{}%", like_escape(&f)));
        }
        let cap = if limit == 0 { 200_000 } else { (limit * 4).max(400) };

        if !q.has_text() {
            let hits = self.browse(&fsql, &fparams, limit)?;
            return Ok(SearchOutcome { hits, note: None });
        }

        let mut note = None;
        let mut rows: Vec<Raw> = Vec::new();
        if let Some(m) = q.fts_and() {
            rows = self.fts_rows(&m, &fsql, &fparams, cap);
        }
        if rows.is_empty() {
            if let Some((terms, msg)) = self.correct_terms(q)? {
                if let Some(m) = q.fts_and_with(&terms) {
                    rows = self.fts_rows(&m, &fsql, &fparams, cap);
                    if !rows.is_empty() {
                        note = Some(msg);
                    }
                }
            }
        }
        if rows.is_empty() {
            if let Some(m) = q.fts_or() {
                rows = self.fts_rows(&m, &fsql, &fparams, cap);
                if !rows.is_empty() {
                    note = Some("no entry has every word — showing partial matches".to_string());
                }
            }
        }
        if rows.is_empty() {
            rows = self.like_rows(q, &fsql, &fparams, cap)?;
        }

        let mut hits: Vec<(Hit, i64)> = rows
            .into_iter()
            .map(|r| (Hit { id: r.id, score: adjust(r.score, r.uses, r.favorite, r.source) }, r.uses))
            .collect();
        hits.sort_by(|a, b| {
            a.0.score
                .partial_cmp(&b.0.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.1.cmp(&a.1))
                .then(a.0.id.cmp(&b.0.id))
        });
        let mut hits: Vec<Hit> = hits.into_iter().map(|h| h.0).collect();
        if limit > 0 {
            hits.truncate(limit);
        }
        Ok(SearchOutcome { hits, note })
    }

    /// Filters only (no words): favorites, most used, yours first, then A–Z.
    fn browse(&self, fsql: &str, fparams: &[String], limit: usize) -> Result<Vec<Hit>> {
        let sql = format!(
            "SELECT e.id FROM entries e WHERE 1=1{}
             ORDER BY e.favorite DESC, e.uses DESC, e.last_used DESC,
                      CASE e.source WHEN 'user' THEN 3 WHEN 'import' THEN 2 WHEN 'pack' THEN 1 ELSE 0 END DESC,
                      e.tool, e.title
             {}",
            fsql,
            if limit > 0 { format!("LIMIT {}", limit) } else { String::new() }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let ids: Vec<i64> = stmt
            .query_map(params_from_iter(fparams.iter()), |r| r.get::<_, i64>(0))?
            .filter_map(|r| r.ok())
            .collect();
        let n = ids.len() as f64;
        Ok(ids.into_iter().enumerate().map(|(i, id)| Hit { id, score: i as f64 - n }).collect())
    }

    fn fts_rows(&self, m: &str, fsql: &str, fparams: &[String], cap: usize) -> Vec<Raw> {
        let sql = format!(
            "SELECT e.id, {bm}, e.uses, e.favorite, e.source
             FROM entries_fts JOIN entries e ON e.id = entries_fts.rowid
             WHERE entries_fts MATCH ?{f}
             ORDER BY 2 LIMIT {cap}",
            bm = BM25,
            f = fsql,
            cap = cap
        );
        let mut all: Vec<String> = Vec::with_capacity(fparams.len() + 1);
        all.push(m.to_string());
        all.extend(fparams.iter().cloned());
        let run = || -> Result<Vec<Raw>> {
            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt
                .query_map(params_from_iter(all.iter()), |r| {
                    Ok(Raw {
                        id: r.get(0)?,
                        score: r.get(1)?,
                        uses: r.get(2)?,
                        favorite: r.get::<_, i64>(3)? != 0,
                        source: Source::from_db(&r.get::<_, String>(4)?),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        };
        run().unwrap_or_default()
    }

    /// Substring scan over every text column — catches flag-shaped queries
    /// (`-p-`, `--script`) that the tokenizer cannot index.
    fn like_rows(&self, q: &Query, fsql: &str, fparams: &[String], cap: usize) -> Result<Vec<Raw>> {
        let mut needles: Vec<String> = q.terms.clone();
        needles.extend(q.phrases.iter().cloned());
        if needles.is_empty() {
            return Ok(vec![]);
        }
        let mut sql = String::from(
            "SELECT e.id, e.uses, e.favorite, e.source FROM entries e WHERE 1=1",
        );
        let mut p: Vec<String> = Vec::new();
        for n in needles.iter().filter(|n| !Query::is_flag_term(n)) {
            sql.push_str(&format!(" AND {} LIKE ? ESCAPE '\\'", HAYSTACK));
            p.push(format!("%{}%", like_escape(n)));
        }
        sql.push_str(fsql);
        p.extend(fparams.iter().cloned());
        sql.push_str(&format!(" ORDER BY e.favorite DESC, e.uses DESC, length(e.title) LIMIT {}", cap));
        let mut stmt = self.conn.prepare(&sql)?;
        let rows: Vec<Raw> = stmt
            .query_map(params_from_iter(p.iter()), |r| {
                Ok(Raw {
                    id: r.get(0)?,
                    score: 0.0,
                    uses: r.get(1)?,
                    favorite: r.get::<_, i64>(2)? != 0,
                    source: Source::from_db(&r.get::<_, String>(3)?),
                })
            })?
            .filter_map(|r| r.ok())
            .collect();
        // keep the SQL order: give descending pseudo-scores
        let n = rows.len() as f64;
        Ok(rows.into_iter().enumerate().map(|(i, mut r)| { r.score = i as f64 - n; r }).collect())
    }

    fn term_has_hits(&self, word: &str) -> bool {
        let m = format!("\"{}\"*", word.replace('"', "\"\""));
        self.conn
            .query_row(
                "SELECT 1 FROM entries_fts WHERE entries_fts MATCH ?1 LIMIT 1",
                params![m],
                |_| Ok(()),
            )
            .optional()
            .map(|o| o.is_some())
            .unwrap_or(true) // on error, assume it matched: don't "correct" blindly
    }

    /// The indexed word closest to `word` (edit distance within budget).
    fn nearest_word(&self, word: &str) -> Result<Option<String>> {
        let budget = typo_budget(word.chars().count());
        if budget == 0 {
            return Ok(None);
        }
        let len = word.chars().count() as i64;
        let mut stmt = self.conn.prepare(
            "SELECT term, doc FROM entries_vocab WHERE length(term) BETWEEN ?1 AND ?2",
        )?;
        let mut best: Option<(usize, i64, String)> = None;
        let rows = stmt.query_map(params![len - budget as i64, len + budget as i64], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for r in rows.flatten() {
            let (term, doc) = r;
            if term == word {
                continue;
            }
            if let Some(d) = osa_distance(word, &term, budget) {
                let better = match &best {
                    None => true,
                    Some((bd, bdoc, _)) => d < *bd || (d == *bd && doc > *bdoc),
                };
                if better {
                    best = Some((d, doc, term));
                }
            }
        }
        Ok(best.map(|b| b.2))
    }

    /// Replace words that match nothing with their nearest indexed word.
    fn correct_terms(&self, q: &Query) -> Result<Option<(Vec<String>, String)>> {
        let mut out = Vec::new();
        let mut changed = Vec::new();
        for t in q.effective_terms() {
            let word: String = t.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect();
            if Query::is_flag_term(&t) || word.chars().count() < 3 || self.term_has_hits(&word) {
                out.push(t);
                continue;
            }
            match self.nearest_word(&word)? {
                Some(w) => {
                    changed.push(format!("{} → {}", t, w));
                    out.push(w);
                }
                None => out.push(t),
            }
        }
        if changed.is_empty() {
            Ok(None)
        } else {
            Ok(Some((out, format!("no match for the typed words — showing results for: {}", changed.join(", ")))))
        }
    }

    // ── variables & meta ─────────────────────────────────────────────────────

    pub fn vars(&self) -> Result<Vec<Var>> {
        let mut stmt = self.conn.prepare("SELECT name, value, pinned FROM vars ORDER BY pinned DESC, name")?;
        let v = stmt
            .query_map([], |r| {
                Ok(Var { name: r.get(0)?, value: r.get(1)?, pinned: r.get::<_, i64>(2)? != 0 })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// Pin a variable: it will be substituted silently from now on.
    pub fn set_var(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO vars(name, value, pinned, updated_at) VALUES (?1, ?2, 1, ?3)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value, pinned = 1, updated_at = excluded.updated_at",
            params![name, value, now_str()],
        )?;
        Ok(())
    }

    /// Remember the last value typed for a prompt (pre-fills next time, never
    /// substituted silently, never overrides a pinned value).
    pub fn remember_var(&self, name: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO vars(name, value, pinned, updated_at) VALUES (?1, ?2, 0, ?3)
             ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at
             WHERE pinned = 0",
            params![name, value, now_str()],
        )?;
        Ok(())
    }

    pub fn unset_var(&self, name: &str) -> Result<bool> {
        Ok(self.conn.execute("DELETE FROM vars WHERE name = ?1", params![name])? > 0)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
            .optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ── pack sync ────────────────────────────────────────────────────────────

    /// Make the pack-managed rows match `entries`, without ever overwriting an
    /// entry you customised or resurrecting one you deleted.
    pub fn sync_pack(&mut self, entries: &[NewEntry]) -> Result<SyncStats> {
        struct Row { id: i64, pack_hash: String, cur_hash: String }

        let now = now_str();
        let mut stats = SyncStats::default();
        let tx = self.conn.transaction()?;

        let mut existing: HashMap<String, Row> = HashMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT id, pack_key, pack_hash, title, tool, command, content, tags, keywords, category, danger
                 FROM entries WHERE pack_key <> ''",
            )?;
            let rows = stmt.query_map([], |r| {
                let tags: String = r.get(7)?;
                let cur = content_hash(
                    &r.get::<_, String>(3)?,
                    &r.get::<_, String>(4)?,
                    &r.get::<_, String>(5)?,
                    &r.get::<_, String>(6)?,
                    &tags,
                    &r.get::<_, String>(8)?,
                    &r.get::<_, String>(9)?,
                    r.get::<_, i64>(10)? != 0,
                );
                Ok((r.get::<_, String>(1)?, Row { id: r.get(0)?, pack_hash: r.get(2)?, cur_hash: cur }))
            })?;
            for r in rows {
                let (k, row) = r?;
                existing.insert(k, row);
            }
        }
        let tombstones: HashSet<String> = {
            let mut stmt = tx.prepare("SELECT key FROM pack_tombstones")?;
            let set = stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<HashSet<_>>>()?;
            set
        };

        let mut seen: HashSet<&str> = HashSet::new();
        {
            let mut ins = tx.prepare(
                "INSERT INTO entries
                   (title, content, category, tags, created_at, updated_at,
                    tool, command, keywords, danger, source, pack_key, pack_hash)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8, ?9, 'pack', ?10, ?11)",
            )?;
            let mut upd = tx.prepare(
                "UPDATE entries SET title=?1, content=?2, category=?3, tags=?4, tool=?5, command=?6,
                    keywords=?7, danger=?8, pack_hash=?9, updated_at=?10 WHERE id=?11",
            )?;
            for e in entries {
                seen.insert(e.pack_key.as_str());
                let h = e.content_hash();
                match existing.get(&e.pack_key) {
                    None => {
                        if tombstones.contains(&e.pack_key) {
                            stats.skipped_deleted += 1;
                            continue;
                        }
                        ins.execute(params![
                            e.title, e.content, e.category.as_str(), e.tags.join(","), now,
                            e.tool, e.command, e.keywords, e.danger as i64, e.pack_key, h
                        ])?;
                        stats.added += 1;
                    }
                    Some(row) => {
                        if row.pack_hash == h {
                            if row.cur_hash == h { stats.unchanged += 1 } else { stats.kept += 1 }
                        } else if row.cur_hash == row.pack_hash {
                            upd.execute(params![
                                e.title, e.content, e.category.as_str(), e.tags.join(","), e.tool,
                                e.command, e.keywords, e.danger as i64, h, now, row.id
                            ])?;
                            stats.updated += 1;
                        } else {
                            stats.kept += 1; // you customised it; the new pack text does not win
                        }
                    }
                }
            }
        }

        // Rows whose key left the pack: drop if untouched, else detach as yours.
        for (key, row) in &existing {
            if seen.contains(key.as_str()) {
                continue;
            }
            if row.cur_hash == row.pack_hash {
                tx.execute("DELETE FROM entries WHERE id = ?1", params![row.id])?;
                tx.execute("DELETE FROM pack_tombstones WHERE key = ?1", params![key])?;
                stats.removed += 1;
            } else {
                tx.execute(
                    "UPDATE entries SET pack_key = '', pack_hash = '', source = 'user' WHERE id = ?1",
                    params![row.id],
                )?;
                stats.kept += 1;
            }
        }

        tx.commit()?;
        Ok(stats)
    }

    /// Forget which pack entries you deleted, so the next sync restores them.
    pub fn clear_pack_tombstones(&self) -> Result<usize> {
        Ok(self.conn.execute("DELETE FROM pack_tombstones", [])?)
    }

    // ── stats & listings ─────────────────────────────────────────────────────

    fn group_count(&self, col: &str) -> Result<Vec<(String, i64)>> {
        let sql = format!("SELECT {c}, count(*) FROM entries GROUP BY {c} ORDER BY 2 DESC", c = col);
        let mut stmt = self.conn.prepare(&sql)?;
        let v = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    pub fn stats(&self) -> Result<Stats> {
        let one = |sql: &str| -> Result<i64> { Ok(self.conn.query_row(sql, [], |r| r.get(0))?) };
        Ok(Stats {
            total: one("SELECT count(*) FROM entries")?,
            by_category: self.group_count("category")?,
            by_source: self.group_count("source")?,
            favorites: one("SELECT count(*) FROM entries WHERE favorite = 1")?,
            with_command: one("SELECT count(*) FROM entries WHERE command <> ''")?,
            dangerous: one("SELECT count(*) FROM entries WHERE danger = 1")?,
            total_uses: one("SELECT coalesce(sum(uses), 0) FROM entries")?,
            distinct_tools: one("SELECT count(DISTINCT tool) FROM entries WHERE tool <> ''")?,
            untooled: one("SELECT count(*) FROM entries WHERE tool = ''")?,
            vars: one("SELECT count(*) FROM vars WHERE pinned = 1")?,
            db_bytes: std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0),
        })
    }

    /// Tools by entry count, most covered first.
    pub fn tool_counts(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        let sql = format!(
            "SELECT tool, count(*) FROM entries WHERE tool <> '' GROUP BY tool ORDER BY 2 DESC, 1 {}",
            if limit > 0 { format!("LIMIT {}", limit) } else { String::new() }
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let v = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(v)
    }

    /// Tags by frequency (they are stored comma-joined, so counted in Rust).
    pub fn tag_counts(&self, limit: usize) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare("SELECT tags FROM entries WHERE tags <> ''")?;
        let mut counts: HashMap<String, i64> = HashMap::new();
        for r in stmt.query_map([], |r| r.get::<_, String>(0))?.flatten() {
            for t in parse_tags(&r) {
                *counts.entry(t.to_lowercase()).or_insert(0) += 1;
            }
        }
        let mut v: Vec<(String, i64)> = counts.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        if limit > 0 {
            v.truncate(limit);
        }
        Ok(v)
    }

    // ── health ───────────────────────────────────────────────────────────────

    /// `(ok, message)` lines for `recall doctor`.
    pub fn health(&self) -> Result<Vec<(bool, String)>> {
        let mut out: Vec<(bool, String)> = Vec::new();

        let integ: String = self.conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        out.push((integ == "ok", format!("SQLite integrity check: {}", integ)));

        let fts_ok = self
            .conn
            .execute("INSERT INTO entries_fts(entries_fts, rank) VALUES('integrity-check', 1)", [])
            .is_ok();
        out.push((fts_ok, format!("search index consistent with entries: {}", if fts_ok { "yes" } else { "NO — run `recall doctor --fix`" })));

        let entries = self.count()?;
        let indexed: i64 = self
            .conn
            .query_row("SELECT count(*) FROM entries_fts_docsize", [], |r| r.get(0))
            .unwrap_or(-1);
        out.push((indexed == entries, format!("indexed rows {} / entries {}", indexed, entries)));

        let empty_titles: i64 =
            self.conn.query_row("SELECT count(*) FROM entries WHERE trim(title) = ''", [], |r| r.get(0))?;
        out.push((empty_titles == 0, format!("entries with an empty title: {}", empty_titles)));

        let dupes: i64 = self.conn.query_row(
            "SELECT coalesce(sum(c - 1), 0) FROM (SELECT count(*) c FROM entries GROUP BY title, content HAVING c > 1)",
            [],
            |r| r.get(0),
        )?;
        out.push((dupes == 0, format!("exact duplicates (title + content): {}{}", dupes, if dupes > 0 { " — see `recall dedupe`" } else { "" })));

        let tomb: i64 = self.conn.query_row("SELECT count(*) FROM pack_tombstones", [], |r| r.get(0))?;
        out.push((true, format!("built-in entries you deleted (kept deleted on updates): {}", tomb)));
        Ok(out)
    }

    /// Rebuild the search index from the entries table.
    pub fn rebuild_fts(&self) -> Result<()> {
        self.conn.execute("INSERT INTO entries_fts(entries_fts) VALUES('rebuild')", [])?;
        Ok(())
    }

    pub fn optimize(&self) -> Result<()> {
        self.conn.execute("INSERT INTO entries_fts(entries_fts) VALUES('optimize')", [])?;
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA optimize;")?;
        Ok(())
    }
}

struct Raw {
    id: i64,
    score: f64,
    uses: i64,
    favorite: bool,
    source: Source,
}

/// De-duplication key for a command: tool plus the command with whitespace
/// collapsed and case folded.
pub fn command_key(tool: &str, command: &str) -> String {
    format!("{}\u{1f}{}", tool.to_lowercase(), squeeze(command).to_lowercase())
}

/// Ids of rows that belong to a bulk-import batch: a run of at least
/// [`MIN_BATCH`] rows whose creation stamps are no more than a second apart.
pub fn detect_import_batches(rows: &[(i64, String)]) -> HashSet<i64> {
    let mut stamped: Vec<(i64, i64)> = rows
        .iter()
        .filter_map(|(id, ts)| {
            chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|t| (t.and_utc().timestamp(), *id))
        })
        .collect();
    stamped.sort();

    let mut out = HashSet::new();
    let mut run: Vec<i64> = Vec::new();
    let mut prev: Option<i64> = None;
    let flush = |run: &mut Vec<i64>, out: &mut HashSet<i64>| {
        if run.len() >= MIN_BATCH {
            out.extend(run.iter().copied());
        }
        run.clear();
    };
    for (ts, id) in stamped {
        if let Some(p) = prev {
            if ts - p > 1 {
                flush(&mut run, &mut out);
            }
        }
        run.push(id);
        prev = Some(ts);
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_db() -> (Database, PathBuf) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("recall_dbtest_{}_{}.db", std::process::id(), nanos));
        let _ = std::fs::remove_file(&p);
        (Database::new(&p).unwrap(), p)
    }

    fn cleanup(p: &Path) {
        for ext in ["", "-wal", "-shm"] {
            let mut s = p.as_os_str().to_owned();
            s.push(ext);
            let _ = std::fs::remove_file(PathBuf::from(s));
        }
    }

    fn cmd(title: &str, tool: &str, command: &str, kw: &str) -> NewEntry {
        NewEntry {
            title: title.into(),
            tool: tool.into(),
            command: command.into(),
            keywords: kw.into(),
            category: Category::Command,
            ..Default::default()
        }
    }

    /// Build a database in the ORIGINAL (pre-v2) schema, exactly as old builds made it.
    fn legacy_db(rows: &[(&str, &str, &str, &str, &str)]) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("recall_legacy_{}_{}.db", std::process::id(), nanos));
        let c = Connection::open(&p).unwrap();
        c.execute_batch(
            "CREATE TABLE entries (
                id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, content TEXT NOT NULL DEFAULT '',
                category TEXT NOT NULL DEFAULT 'note', tags TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, favorite INTEGER NOT NULL DEFAULT 0);
             CREATE VIRTUAL TABLE entries_fts USING fts5(title, tags, content, content='entries', content_rowid='id');
             CREATE TRIGGER entries_ai AFTER INSERT ON entries BEGIN
               INSERT INTO entries_fts(rowid, title, tags, content) VALUES (new.id, new.title, new.tags, new.content); END;",
        )
        .unwrap();
        for (title, content, cat, tags, ts) in rows {
            c.execute(
                "INSERT INTO entries (title, content, category, tags, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?5)",
                params![title, content, cat, tags, ts],
            )
            .unwrap();
        }
        p
    }

    #[test]
    fn fresh_database_is_v2_with_all_objects() {
        let (db, p) = tmp_db();
        let v: i64 = db.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        for t in ["entries", "entries_fts", "entries_vocab", "vars", "meta", "pack_tombstones"] {
            assert!(db.table_exists(t).unwrap(), "missing {}", t);
        }
        assert!(db.migration_note.is_none());
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn legacy_database_migrates_additively_and_keeps_every_row() {
        let p = legacy_db(&[
            ("Basics (3)", "```bash\nuniq file.txt\n```", "command", "linux,misc", "2026-09-08 17:00:00"),
            ("Kerberoast", "use GetUserSPNs", "note", "ad", "2026-09-17 14:00:00"),
        ]);
        let db = Database::new(&p).unwrap();
        assert_eq!(db.count().unwrap(), 2);
        let all = db.get_all_entries().unwrap();
        let basics = all.iter().find(|e| e.title == "Basics (3)").unwrap();
        assert_eq!(basics.tool, "uniq", "tool derived from the first fenced command");
        assert_eq!(basics.content, "```bash\nuniq file.txt\n```", "content untouched");
        assert_eq!(basics.tags, vec!["linux", "misc"]);
        assert!(db.migration_note.as_deref().unwrap().contains("backup"));

        // the backup exists and is a readable v0/v1 copy with the same rows
        let note = db.migration_note.clone().unwrap();
        let bak = note.split("backup: ").nth(1).unwrap().trim_end_matches(')');
        let bc = Connection::open(bak).unwrap();
        let n: i64 = bc.query_row("SELECT count(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let v: i64 = bc.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(v, 0, "the backup is the untouched pre-migration file");

        // search works on the rebuilt index, including the new `tool` column
        assert_eq!(db.search_ids("tool:uniq").unwrap().len(), 1);
        assert_eq!(db.search_ids("kerberoast").unwrap().len(), 1);

        drop(db);
        drop(bc);
        cleanup(&p);
        let _ = std::fs::remove_file(bak);
    }

    #[test]
    fn reopening_a_migrated_database_is_a_noop() {
        let p = legacy_db(&[("a", "b", "note", "", "2026-01-01 00:00:00")]);
        let bak;
        {
            let db = Database::new(&p).unwrap();
            bak = db.migration_note.clone().unwrap().split("backup: ").nth(1).unwrap().trim_end_matches(')').to_string();
        }
        let db2 = Database::new(&p).unwrap();
        assert!(db2.migration_note.is_none(), "no second backup / migration");
        assert_eq!(db2.count().unwrap(), 1);
        drop(db2);
        cleanup(&p);
        let _ = std::fs::remove_file(bak);
    }

    #[test]
    fn newer_schema_is_refused_rather_than_damaged() {
        let (db, p) = tmp_db();
        db.conn.execute_batch("PRAGMA user_version = 99").unwrap();
        drop(db);
        let err = Database::new(&p).err().expect("must refuse");
        assert!(err.to_string().contains("newer") || err.to_string().contains("upgrade recall"));
        cleanup(&p);
    }

    #[test]
    fn batch_detection_separates_imports_from_hand_made_rows() {
        let mut rows: Vec<(i64, String)> = (0..30)
            .map(|i| (i as i64 + 1, format!("2026-09-08 17:{:02}:{:02}", i / 60, i % 60)))
            .collect();
        rows.push((100, "2026-09-17 14:03:11".into()));
        rows.push((101, "2026-09-17 14:03:12".into())); // only 2 in a row: not a batch
        rows.push((102, "garbage".into()));
        let b = detect_import_batches(&rows);
        assert_eq!(b.len(), 30);
        assert!(!b.contains(&100) && !b.contains(&101) && !b.contains(&102));
    }

    #[test]
    fn search_ranks_tool_and_title_above_incidental_mentions() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("Scan every TCP port", "nmap", "nmap -p- {{target}}", "all ports full")).unwrap();
        db.add_entry("Networking notes", "sometimes people run nmap here", Category::Note, &[]).unwrap();
        let hits = db.search_entries("nmap", 10).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].tool, "nmap", "the entry about nmap outranks a passing mention");
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn keywords_make_intent_queries_findable() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("List listening sockets", "ss", "ss -tulpn", "open ports what is listening")).unwrap();
        assert_eq!(db.search_entries("what is listening on my ports", 5).unwrap().len(), 1);
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn filters_narrow_and_work_without_words() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("a", "nmap", "nmap x", "")).unwrap();
        db.add_new(&NewEntry { tags: vec!["web".into()], ..cmd("b", "gobuster", "gobuster dir", "") }).unwrap();
        let id = db.add_new(&NewEntry { danger: true, ..cmd("c", "dd", "dd if=a of=b", "") }).unwrap();
        db.set_favorite(id, true).unwrap();
        assert_eq!(db.search_ids("tool:nmap").unwrap().len(), 1);
        assert_eq!(db.search_ids("tool:go").unwrap().len(), 1, "tool: matches by prefix");
        assert_eq!(db.search_ids("#web").unwrap().len(), 1);
        assert_eq!(db.search_ids("is:danger").unwrap(), vec![id]);
        assert_eq!(db.search_ids("is:fav").unwrap(), vec![id]);
        assert_eq!(db.search_ids("cat:command").unwrap().len(), 3);
        assert_eq!(db.search_ids("cat:note").unwrap().len(), 0);
        assert_eq!(db.search_ids("nmap tool:gobuster").unwrap().len(), 0, "filters AND with words");
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn typos_are_corrected_and_reported() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("Brute force directories", "gobuster", "gobuster dir -u {{url}}", "")).unwrap();
        let q = Query::parse("gobsuter");
        let out = db.search(&q, 5).unwrap();
        assert_eq!(out.hits.len(), 1, "gobsuter → gobuster");
        assert!(out.note.unwrap().contains("gobuster"));
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn partial_matches_fall_back_to_or() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("Kill process by name", "pkill", "pkill {{name}}", "")).unwrap();
        let out = db.search(&Query::parse("kill zzzunmatchable"), 5).unwrap();
        assert_eq!(out.hits.len(), 1);
        assert!(out.note.unwrap().contains("partial"));
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn flag_shaped_queries_use_the_substring_fallback() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("Ping sweep", "nmap", "nmap -sn 10.0.0.0/24", "")).unwrap();
        assert_eq!(db.search_ids("--- ~~~").unwrap().len(), 0);
        assert_eq!(db.search_ids("10.0.0.0/24").unwrap().len(), 1);
        db.add_entry("port scan", "nmap -p-", Category::Command, &[]).unwrap();
        assert_eq!(db.search_ids("-p-").unwrap().len(), 1, "-p- is an exact substring, not p*");
        db.add_new(&cmd("Service scan", "nmap", "nmap -sV x", "")).unwrap();
        assert_eq!(db.search_ids("nmap -sV").unwrap().len(), 1, "word + flag combine");
        assert_eq!(db.search_ids("nmap").unwrap().len(), 3);
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn usage_and_favorites_boost_ranking() {
        let (db, p) = tmp_db();
        let a = db.add_new(&cmd("copy files", "cp", "cp a b", "")).unwrap();
        let b = db.add_new(&cmd("copy files", "rsync", "rsync a b", "")).unwrap();
        let first = db.search_ids("copy files").unwrap();
        for _ in 0..5 {
            db.record_use(if first[0] == a { b } else { a }).unwrap();
        }
        let after = db.search_ids("copy files").unwrap();
        assert_ne!(first[0], after[0], "the entry you keep using moves up");
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn source_prior_puts_your_entries_above_tldr() {
        let (db, p) = tmp_db();
        db.add_new(&NewEntry { source: Source::Tldr, ..cmd("Archive files", "tar", "tar cf a.tar x", "") }).unwrap();
        let mine = db.add_new(&NewEntry { source: Source::User, ..cmd("Archive files", "tar", "tar czf a.tgz x", "") }).unwrap();
        assert_eq!(db.search_ids("archive files").unwrap()[0], mine);
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn fts_index_stays_in_sync_for_new_columns() {
        let (db, p) = tmp_db();
        let id = db.add_new(&cmd("t", "nmap", "nmap uniquecmdword", "kwuniqueword")).unwrap();
        assert_eq!(db.search_ids("uniquecmdword").unwrap(), vec![id]);
        assert_eq!(db.search_ids("kwuniqueword").unwrap(), vec![id]);
        db.update_full(id, &NewEntry { command: "nmap otherword".into(), ..cmd("t", "nmap", "", "kwuniqueword") }).unwrap();
        assert!(db.search_ids("uniquecmdword").unwrap().is_empty(), "old command de-indexed");
        assert_eq!(db.search_ids("otherword").unwrap(), vec![id]);
        db.delete_entry(id).unwrap();
        assert!(db.search_ids("otherword").unwrap().is_empty());
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn restore_preserves_usage_source_and_pack_identity() {
        let (db, p) = tmp_db();
        let id = db.add_new(&NewEntry { pack_key: "nmap/x".into(), source: Source::Pack, ..cmd("x", "nmap", "nmap x", "") }).unwrap();
        db.record_use(id).unwrap();
        db.set_favorite(id, true).unwrap();
        let e = db.get_entry(id).unwrap().unwrap();
        let snap = DeletedEntry {
            title: e.title.clone(), content: e.content.clone(), category: e.category.clone(), tags: e.tags.clone(),
            favorite: e.favorite, created_at: e.created_at.clone(), updated_at: e.updated_at.clone(),
            tool: e.tool.clone(), command: e.command.clone(), keywords: e.keywords.clone(), danger: e.danger,
            uses: e.uses, last_used: e.last_used.clone(), source: e.source, pack_key: e.pack_key.clone(),
            pack_hash: e.pack_hash.clone(),
        };
        db.delete_entry(id).unwrap();
        let nid = db.restore_entry(&snap).unwrap();
        let r = db.get_entry(nid).unwrap().unwrap();
        assert_eq!((r.uses, r.favorite, r.source, r.pack_key.as_str()), (1, true, Source::Pack, "nmap/x"));
        drop(db);
        cleanup(&p);
    }

    fn pack_entry(key: &str, title: &str, command: &str) -> NewEntry {
        NewEntry {
            title: title.into(),
            tool: "nmap".into(),
            command: command.into(),
            category: Category::Command,
            source: Source::Pack,
            pack_key: key.into(),
            ..Default::default()
        }
    }

    #[test]
    fn pack_sync_adds_then_is_idempotent() {
        let (mut db, p) = tmp_db();
        let pack = vec![pack_entry("nmap/a", "A", "nmap a"), pack_entry("nmap/b", "B", "nmap b")];
        let s1 = db.sync_pack(&pack).unwrap();
        assert_eq!((s1.added, s1.unchanged), (2, 0));
        let s2 = db.sync_pack(&pack).unwrap();
        assert_eq!((s2.added, s2.updated, s2.unchanged), (0, 0, 2));
        assert_eq!(db.count().unwrap(), 2);
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn pack_sync_updates_untouched_rows_but_never_overwrites_your_edits() {
        let (mut db, p) = tmp_db();
        db.sync_pack(&[pack_entry("nmap/a", "A", "nmap a"), pack_entry("nmap/b", "B", "nmap b")]).unwrap();
        let ids: Vec<Entry> = db.get_all_entries().unwrap();
        let a = ids.iter().find(|e| e.title == "A").unwrap().id;

        // you customise A
        db.update_full(a, &NewEntry { title: "A (mine)".into(), command: "nmap mine".into(), category: Category::Command, tool: "nmap".into(), ..Default::default() }).unwrap();

        // the pack ships better text for both
        let s = db.sync_pack(&[pack_entry("nmap/a", "A", "nmap a --better"), pack_entry("nmap/b", "B", "nmap b --better")]).unwrap();
        assert_eq!((s.updated, s.kept), (1, 1));
        let after = db.get_all_entries().unwrap();
        assert_eq!(after.iter().find(|e| e.id == a).unwrap().command, "nmap mine", "your edit survives");
        assert_eq!(after.iter().find(|e| e.title == "B").unwrap().command, "nmap b --better", "untouched entry updated");
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn pack_sync_does_not_resurrect_deleted_entries_until_asked() {
        let (mut db, p) = tmp_db();
        let pack = vec![pack_entry("nmap/a", "A", "nmap a"), pack_entry("nmap/b", "B", "nmap b")];
        db.sync_pack(&pack).unwrap();
        let a = db.get_all_entries().unwrap().into_iter().find(|e| e.title == "A").unwrap().id;
        db.delete_entry(a).unwrap();

        let s = db.sync_pack(&pack).unwrap();
        assert_eq!((s.added, s.skipped_deleted), (0, 1));
        assert_eq!(db.count().unwrap(), 1);

        db.clear_pack_tombstones().unwrap();
        let s = db.sync_pack(&pack).unwrap();
        assert_eq!(s.added, 1, "restored on request");
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn pack_sync_removes_retired_entries_but_detaches_customised_ones() {
        let (mut db, p) = tmp_db();
        db.sync_pack(&[pack_entry("nmap/a", "A", "nmap a"), pack_entry("nmap/b", "B", "nmap b")]).unwrap();
        let b = db.get_all_entries().unwrap().into_iter().find(|e| e.title == "B").unwrap().id;
        db.update_full(b, &NewEntry { title: "B".into(), command: "nmap edited".into(), category: Category::Command, tool: "nmap".into(), ..Default::default() }).unwrap();

        let s = db.sync_pack(&[]).unwrap(); // both retired from the pack
        assert_eq!((s.removed, s.kept), (1, 1));
        let left = db.get_all_entries().unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!((left[0].source, left[0].pack_key.as_str()), (Source::User, ""), "detached and now yours");
        // and retiring an entry must not tombstone its key forever
        let s = db.sync_pack(&[pack_entry("nmap/a", "A", "nmap a")]).unwrap();
        assert_eq!(s.added, 1);
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn vars_pin_and_remember_semantics() {
        let (db, p) = tmp_db();
        db.remember_var("file", "a.txt").unwrap();
        db.set_var("target", "10.0.0.5").unwrap();
        db.remember_var("target", "SHOULD-NOT-WIN").unwrap();
        let v = db.vars().unwrap();
        let t = v.iter().find(|x| x.name == "target").unwrap();
        assert_eq!((t.value.as_str(), t.pinned), ("10.0.0.5", true), "remembering never overrides a pinned value");
        assert!(!v.iter().find(|x| x.name == "file").unwrap().pinned);
        assert!(db.unset_var("target").unwrap());
        assert!(!db.unset_var("target").unwrap());
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn health_is_clean_on_a_fresh_database_and_detects_a_stale_index() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("x", "ls", "ls", "")).unwrap();
        assert!(db.health().unwrap().iter().all(|(ok, m)| *ok || m.contains("duplicates")), "{:?}", db.health().unwrap());
        // corrupt the index by bypassing the triggers
        db.conn.execute_batch("DROP TRIGGER entries_ai; INSERT INTO entries(title,created_at,updated_at) VALUES('sneaky','x','x');").unwrap();
        let h = db.health().unwrap();
        assert!(h.iter().any(|(ok, m)| !*ok && m.contains("indexed rows")), "{:?}", h);
        db.rebuild_fts().unwrap();
        let h = db.health().unwrap();
        assert!(h.iter().any(|(ok, m)| *ok && m.contains("indexed rows")), "{:?}", h);
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn stats_and_listings() {
        let (db, p) = tmp_db();
        db.add_new(&NewEntry { tags: vec!["Web".into(), "recon".into()], ..cmd("a", "nmap", "nmap a", "") }).unwrap();
        db.add_new(&NewEntry { tags: vec!["web".into()], ..cmd("b", "nmap", "nmap b", "") }).unwrap();
        db.add_entry("note", "text", Category::Note, &[]).unwrap();
        let s = db.stats().unwrap();
        assert_eq!((s.total, s.with_command, s.distinct_tools, s.untooled), (3, 2, 1, 1));
        assert_eq!(db.tool_counts(0).unwrap(), vec![("nmap".to_string(), 2)]);
        assert_eq!(db.tag_counts(0).unwrap()[0], ("web".to_string(), 2), "tags are counted case-insensitively");
        drop(db);
        cleanup(&p);
    }

    #[test]
    fn backup_writes_a_readable_copy_and_refuses_to_overwrite() {
        let (db, p) = tmp_db();
        db.add_new(&cmd("x", "ls", "ls", "")).unwrap();
        let mut dest = std::env::temp_dir();
        dest.push(format!("recall_bak_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dest);
        db.backup_to(&dest).unwrap();
        let c = Connection::open(&dest).unwrap();
        let n: i64 = c.query_row("SELECT count(*) FROM entries", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(db.backup_to(&dest).is_err());
        drop(c);
        let _ = std::fs::remove_file(&dest);
        drop(db);
        cleanup(&p);
    }
}
