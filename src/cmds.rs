//! Implementations for every non-interactive-TUI CLI subcommand.
//!
//! Conventions used throughout: `stdout` carries only the thing being asked
//! for (a command, a list, JSON) so output stays pipeable; prompts, status
//! and warnings go to `stderr`. Nothing here launches the TUI — see `main.rs`.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{bail, Result};

use crate::db::Database;
use crate::fill;
use crate::models::{Category, Entry, NewEntry, Source};
use crate::query::Query;
use crate::util::{self, Paint};

// ─── shared: variables, resolution, matching ─────────────────────────────────

/// Load every stored variable into a fill context (pinned ones silently
/// substitute; every value also pre-fills its prompt as a suggestion).
pub fn load_ctx(db: &Database) -> Result<fill::Ctx> {
    let mut ctx = fill::Ctx::default();
    for v in db.vars()? {
        ctx.last.insert(v.name.clone(), v.value.clone());
        if v.pinned {
            ctx.pinned.insert(v.name, v.value);
        }
    }
    Ok(ctx)
}

/// Resolve every placeholder in `cmd`. Non-interactive: known/suggested values
/// are used, the rest stay as written and their labels are returned so the
/// caller can warn about them.
pub fn resolve_quiet(cmd: &str, ctx: &fill::Ctx) -> (String, Vec<String>) {
    fill::fill_quiet(cmd, &HashMap::new(), ctx)
}

/// Resolve every placeholder, prompting on stderr/stdin for anything left
/// open when `interactive`. Values you type are remembered (unless a pinned
/// variable already covers that key). Falls back to [`resolve_quiet`] when
/// not interactive or stdin is not a terminal.
pub fn resolve_interactive(db: &Database, cmd: &str, ctx: &fill::Ctx, interactive: bool) -> Result<String> {
    let r = fill::resolve(cmd, &HashMap::new(), ctx);
    if r.pending.is_empty() {
        return Ok(r.text);
    }
    if !interactive || !util::stdin_is_tty() {
        let (text, unresolved) = resolve_quiet(cmd, ctx);
        if !unresolved.is_empty() {
            eprintln!("(left as-is: {})", unresolved.join(", "));
        }
        return Ok(text);
    }

    let mut typed: HashMap<String, String> = HashMap::new();
    let stdin = std::io::stdin();
    for p in &r.pending {
        let hint = p.suggest.as_deref().map(|s| format!(" [{}]", s)).unwrap_or_default();
        eprint!("{}{}: ", p.label, hint);
        std::io::stderr().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            break; // stdin closed mid-prompt
        }
        let v = line.trim().to_string();
        if !v.is_empty() {
            db.remember_var(&p.key, &v).ok();
        }
        typed.insert(p.key.clone(), v);
    }
    Ok(fill::finish(&r.text, &r.pending, &typed))
}

/// Best match for a query, plus how many other results existed.
pub fn best_match(db: &Database, words: &[String]) -> Result<Option<(Entry, usize)>> {
    let q = Query::from_words(words);
    let out = db.search(&q, 20)?;
    if let Some(note) = &out.note {
        eprintln!("({})", note);
    }
    match out.hits.first() {
        None => Ok(None),
        Some(h) => {
            let e = db.get_entry(h.id)?.expect("hit id must exist");
            Ok(Some((e, out.hits.len() - 1)))
        }
    }
}

fn no_match(words: &[String]) -> anyhow::Error {
    anyhow::anyhow!("no entry matches: {}", words.join(" "))
}

// ─── search / show ────────────────────────────────────────────────────────────

/// `recall search` — the original, pipe-stable format: one line per hit,
/// `[CATEGORY] title  |  tags`.
pub fn search(db: &Database, query: &str) -> Result<()> {
    let entries = db.search_entries(query, 0)?;
    let mut out = std::io::stdout();
    if entries.is_empty() {
        let _ = writeln!(out, "No results for: {}", query);
    } else {
        for e in &entries {
            if writeln!(out, "[{}] {}  |  {}", e.category.label(), e.title, e.tags_display()).is_err() {
                break; // reader closed the pipe (e.g. | head) — exit cleanly
            }
        }
    }
    Ok(())
}

/// `recall show` / the bare `recall <words>` shorthand — a readable listing
/// with each entry's command front and center.
pub fn show(db: &Database, words: &[String], limit: usize, paint: Paint) -> Result<()> {
    let q = Query::from_words(words);
    let out = db.search(&q, limit)?;
    let mut stdout = std::io::stdout();
    if let Some(note) = &out.note {
        eprintln!("({})", note);
    }
    if out.hits.is_empty() {
        if q.is_empty() {
            writeln!(stdout, "No entries yet — `recall add` or `recall pack sync` to get started.")?;
        } else {
            writeln!(stdout, "No matches for: {}", words.join(" "))?;
        }
        return Ok(());
    }
    let terms = q.highlight_terms();
    for (i, h) in out.hits.iter().enumerate() {
        let Some(e) = db.get_entry(h.id)? else { continue };
        if i > 0 {
            writeln!(stdout)?;
        }
        write_entry(&mut stdout, &e, paint, &terms)?;
    }
    Ok(())
}

/// Bold the spans of `text` that case-insensitively match one of `terms`.
fn highlight(text: &str, terms: &[String], p: Paint) -> String {
    if terms.is_empty() || !p.on {
        return text.to_string();
    }
    let lower = text.to_lowercase();
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let lchars: Vec<char> = lower.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let hit = terms.iter().find_map(|t| {
            let tc: Vec<char> = t.chars().collect();
            let n = tc.len();
            (n > 0 && i + n <= lchars.len() && lchars[i..i + n] == tc[..]).then_some(n)
        });
        match hit {
            Some(n) => {
                out.push_str(&p.bold(&chars[i..i + n].iter().collect::<String>()));
                i += n;
            }
            None => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

fn write_entry(out: &mut impl Write, e: &Entry, p: Paint, terms: &[String]) -> Result<()> {
    // Built from independently self-contained (open…reset) ANSI spans rather
    // than one wrapped in another — a reset inside a reset would cut the
    // outer style short for everything after it.
    let star = if e.favorite { " ★" } else { "" };
    let title = highlight(&e.title, terms, p);
    let badge = p.bold(&format!("[{}]", e.category.label()));
    let head = if e.tool.is_empty() {
        format!("{} {}{}", badge, title, star)
    } else {
        format!("{} {} › {}{}", badge, e.tool, title, star)
    };
    writeln!(out, "{}", head)?;
    if e.has_command() {
        writeln!(out, "  {}", p.green(&e.primary_command()))?;
    }
    if !e.content.trim().is_empty() && (!e.has_command() || !e.content.contains(e.command.trim())) {
        for line in e.content.lines().take(6) {
            writeln!(out, "  {}", p.dim(line))?;
        }
    }
    let mut meta = Vec::new();
    if !e.tags.is_empty() {
        meta.push(format!("tags: {}", e.tags_display()));
    }
    meta.push(format!("id: {}", e.id));
    if e.uses > 0 {
        meta.push(format!("used {}×", e.uses));
    }
    writeln!(out, "  {}", p.dim(&meta.join("  ·  ")))?;
    if e.danger {
        let reason = crate::danger::danger_reason(&e.primary_command()).unwrap_or("marked dangerous");
        writeln!(out, "  {} {}", p.red("⚠"), p.red(reason))?;
    }
    Ok(())
}

// ─── cmd / copy / run ─────────────────────────────────────────────────────────

/// `recall cmd` — print only the resolved command. Never prompts, so it is
/// safe inside `$(...)` and shell functions.
pub fn cmd(db: &Database, words: &[String]) -> Result<()> {
    let Some((e, others)) = best_match(db, words)? else { bail!(no_match(words)) };
    if !e.has_command() {
        bail!("'{}' has no command (it's a {})", e.title, e.category.label().to_lowercase());
    }
    if others > 0 {
        eprintln!("({} more match — `recall pick {:?}` to choose)", others, words.join(" "));
    }
    let ctx = load_ctx(db)?;
    let (text, unresolved) = resolve_quiet(&e.primary_command(), &ctx);
    if !unresolved.is_empty() {
        eprintln!("(left as-is: {} — `recall copy` fills these interactively)", unresolved.join(", "));
    }
    println!("{}", text);
    Ok(())
}

/// `recall copy` — resolve (prompting on a terminal), copy to the clipboard,
/// count it as a use.
pub fn copy(db: &Database, words: &[String]) -> Result<()> {
    let Some((e, others)) = best_match(db, words)? else { bail!(no_match(words)) };
    if !e.has_command() {
        bail!("'{}' has no command to copy", e.title);
    }
    if others > 0 {
        eprintln!("({} other matches for {:?})", others, words.join(" "));
    }
    let ctx = load_ctx(db)?;
    let text = resolve_interactive(db, &e.primary_command(), &ctx, true)?;
    match crate::clipboard::copy(&text) {
        Some(tool) => eprintln!("copied via {}: {}", tool, text),
        None => {
            eprintln!("{}", crate::clipboard::MISSING_HINT);
            println!("{}", text);
        }
    }
    db.record_use(e.id)?;
    Ok(())
}

/// `recall run` — resolve, confirm, execute in the user's shell. Refuses
/// outright without a terminal to confirm in.
pub fn run(db: &Database, words: &[String]) -> Result<()> {
    if !util::stdin_is_tty() {
        bail!("refusing to run without a terminal to confirm in — use `recall cmd` and run it yourself");
    }
    let Some((e, others)) = best_match(db, words)? else { bail!(no_match(words)) };
    if !e.has_command() {
        bail!("'{}' has no command to run", e.title);
    }
    if others > 0 {
        eprintln!("({} other matches for {:?})", others, words.join(" "));
    }
    let ctx = load_ctx(db)?;
    let text = resolve_interactive(db, &e.primary_command(), &ctx, true)?;

    let ep = Paint::for_stderr();
    let danger = e.danger || crate::danger::danger_reason(&text).is_some();
    eprintln!("  {}", ep.green(&text));
    if danger {
        let reason = crate::danger::danger_reason(&text).unwrap_or("marked dangerous");
        eprintln!("{}  {} — this looks destructive.", ep.red("⚠"), ep.red(reason));
        eprint!("Type \"yes\" to run it anyway: ");
    } else {
        eprint!("{} ", ep.yellow("Run this? [y/N]"));
    }
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let answer = line.trim().to_lowercase();
    let confirmed = if danger { answer == "yes" } else { matches!(answer.as_str(), "y" | "yes") };
    if !confirmed {
        eprintln!("cancelled.");
        return Ok(());
    }

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let status = std::process::Command::new(&shell).arg("-c").arg(&text).status();
    db.record_use(e.id)?;
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => bail!("exited with {}", s),
        Err(err) => bail!("failed to launch {}: {}", shell, err),
    }
}

// ─── export ───────────────────────────────────────────────────────────────────

/// `recall export` — every entry as one JSON object per line (JSONL) to
/// stdout: a portable, greppable, `jq`-able backup independent of SQLite.
pub fn export(db: &Database) -> Result<()> {
    let mut out = std::io::stdout();
    for e in db.get_all_entries()? {
        let fields = [
            format!("\"id\":{}", e.id),
            format!("\"title\":{}", util::json_str(&e.title)),
            format!("\"content\":{}", util::json_str(&e.content)),
            format!("\"category\":{}", util::json_str(e.category.as_str())),
            format!("\"tags\":{}", util::json_arr(&e.tags)),
            format!("\"favorite\":{}", e.favorite),
            format!("\"tool\":{}", util::json_str(&e.tool)),
            format!("\"command\":{}", util::json_str(&e.command)),
            format!("\"keywords\":{}", util::json_str(&e.keywords)),
            format!("\"danger\":{}", e.danger),
            format!("\"uses\":{}", e.uses),
            format!("\"source\":{}", util::json_str(e.source.as_str())),
            format!("\"created_at\":{}", util::json_str(&e.created_at)),
            format!("\"updated_at\":{}", util::json_str(&e.updated_at)),
        ];
        if writeln!(out, "{{{}}}", fields.join(",")).is_err() {
            break; // reader closed the pipe
        }
    }
    Ok(())
}

// ─── variables ────────────────────────────────────────────────────────────────

pub fn set_var(db: &Database, name: &str, value: &str) -> Result<()> {
    db.set_var(&fill::canon(name), value)?;
    println!("{} = {}", fill::canon(name), value);
    Ok(())
}

pub fn unset_var(db: &Database, name: &str) -> Result<()> {
    if db.unset_var(&fill::canon(name))? {
        println!("unset {}", fill::canon(name));
    } else {
        println!("(not set: {})", fill::canon(name));
    }
    Ok(())
}

pub fn list_vars(db: &Database, paint: Paint) -> Result<()> {
    let vars = db.vars()?;
    if vars.is_empty() {
        println!("No variables set. `recall set target 10.10.11.5` pins one.");
        return Ok(());
    }
    for v in vars {
        let tag = if v.pinned { paint.green("pinned") } else { paint.dim("remembered") };
        println!("{:<12} {:<28} {}", v.name, v.value, tag);
    }
    println!();
    println!("{}", paint.dim(&format!("built-in (always available): {}", fill::BUILTIN_NAMES.join(", "))));
    Ok(())
}

// ─── listings & stats ─────────────────────────────────────────────────────────

pub fn tools(db: &Database, limit: usize) -> Result<()> {
    let rows = db.tool_counts(limit)?;
    if rows.is_empty() {
        println!("No entries have a tool set yet.");
    }
    let width = rows.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(4);
    for (tool, n) in rows {
        println!("{:width$}  {}", tool, n, width = width);
    }
    Ok(())
}

pub fn tags(db: &Database, limit: usize) -> Result<()> {
    let rows = db.tag_counts(limit)?;
    let width = rows.iter().map(|(t, _)| t.chars().count()).max().unwrap_or(4);
    for (tag, n) in rows {
        println!("{:width$}  {}", tag, n, width = width);
    }
    Ok(())
}

pub fn stats(db: &Database, paint: Paint) -> Result<()> {
    let s = db.stats()?;
    println!("{}", paint.bold("recall database"));
    println!("  entries        {}", s.total);
    for (c, n) in &s.by_category {
        println!("    {:<10} {}", c, n);
    }
    println!("  sources");
    for (src, n) in &s.by_source {
        println!("    {:<10} {}", src, n);
    }
    println!("  with a command   {}", s.with_command);
    println!("  distinct tools   {} ({} entries untagged with a tool)", s.distinct_tools, s.untooled);
    println!("  favorites        {}", s.favorites);
    println!("  dangerous        {}", s.dangerous);
    println!("  total uses       {}", s.total_uses);
    println!("  pinned variables {}", s.vars);
    println!("  database size    {}", human_bytes(s.db_bytes));
    if let Some(t) = db.meta_get("pack_synced_at")? {
        println!("  pack last synced {}", t);
    }
    if let Some(t) = db.meta_get("tldr_synced_at")? {
        println!("  tldr last synced {}", t);
    }
    Ok(())
}

fn human_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{} {}", n, units[0])
    } else {
        format!("{:.1} {}", v, units[u])
    }
}

// ─── backup / doctor ────────────────────────────────────────────────────────

/// `recall prune` — remove bulk-imported entries (by default `src:import`),
/// leaving your own hand-written entries and the built-in pack untouched. Reports
/// only unless `--apply` is given, and `--apply` makes a timestamped backup first
/// because the delete is permanent.
pub fn prune(db: &mut Database, source: Source, tag: Option<&str>, apply: bool, paint: Paint) -> Result<()> {
    let breakdown = db.source_breakdown(source, tag)?;
    let total: i64 = breakdown.iter().map(|(_, n)| n).sum();
    let what = match tag {
        Some(t) => format!("{} entries tagged '{}'", source.as_str(), t),
        None => format!("{} entries", source.as_str()),
    };

    if total == 0 {
        println!("No {} to remove.", what);
        return Ok(());
    }

    let parts: Vec<String> = breakdown.iter().map(|(c, n)| format!("{} {}", n, c.as_str())).collect();
    let remaining = db.count()? - total;
    println!(
        "{} {} ({}) · {} would remain",
        if apply { "Removing" } else { "Would remove" },
        paint.bold(&total.to_string()),
        parts.join(", "),
        remaining,
    );

    if apply {
        backup(db, None)?; // permanent delete — keep an escape hatch
        let removed = db.delete_by_source(source, tag)?;
        println!("Removed {} {}. Re-import with `recall import <file>` when ready.", removed, what);
    } else {
        println!("\nNothing was written. Re-run with --apply to delete (a backup is made first).");
    }
    Ok(())
}

pub fn backup(db: &Database, dest: Option<&Path>) -> Result<()> {
    let path = match dest {
        Some(p) => p.to_path_buf(),
        None => {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
            let mut name = db.path.as_os_str().to_owned();
            name.push(format!(".{}.bak", stamp));
            std::path::PathBuf::from(name)
        }
    };
    db.backup_to(&path)?;
    println!("Backed up to {}", path.display());
    Ok(())
}

pub fn doctor(db: &mut Database, fix: bool, paint: Paint) -> Result<()> {
    let mut checks = db.health()?;
    let mut all_ok = checks.iter().all(|(ok, _)| *ok);
    if fix && !all_ok {
        db.rebuild_fts()?;
        checks = db.health()?;
        all_ok = checks.iter().all(|(ok, _)| *ok);
    }
    for (ok, msg) in &checks {
        let mark = if *ok { paint.green("✓") } else { paint.red("✗") };
        println!("{} {}", mark, msg);
    }
    if !all_ok && !fix {
        println!("\nRun `recall doctor --fix` to rebuild the search index.");
    } else if all_ok {
        db.optimize().ok();
        println!("\n{}", paint.green("Everything checks out."));
    }
    Ok(())
}

// ─── pack sync ────────────────────────────────────────────────────────────────

pub fn pack_sync(db: &mut Database, dry_run: bool, paint: Paint) -> Result<()> {
    let entries = crate::pack::builtin_entries();
    if dry_run {
        println!(
            "Built-in pack: {} entries across {} tools. Run without --dry-run to sync.",
            entries.len(),
            entries.iter().map(|e| e.tool.as_str()).collect::<std::collections::HashSet<_>>().len()
        );
        return Ok(());
    }
    let s = db.sync_pack(&entries)?;
    db.meta_set("pack_synced_at", &util::now_str())?;
    println!("{}", paint.bold("Pack sync"));
    println!("  added     {}", s.added);
    println!("  updated   {}", s.updated);
    println!("  unchanged {}", s.unchanged);
    println!("  kept (your edits preserved)  {}", s.kept);
    if s.removed > 0 {
        println!("  removed (retired from the pack) {}", s.removed);
    }
    if s.skipped_deleted > 0 {
        println!(
            "  skipped {} you previously deleted (`recall pack restore` brings them back)",
            s.skipped_deleted
        );
    }
    Ok(())
}

pub fn pack_restore(db: &Database) -> Result<()> {
    let n = db.clear_pack_tombstones()?;
    println!("Forgot {} deletion(s) — the next `recall pack sync` will restore them.", n);
    Ok(())
}

// ─── tldr ─────────────────────────────────────────────────────────────────────

pub fn tldr_status() -> Result<()> {
    match crate::tldr::find_cache() {
        Some(dir) => println!("Found a tldr cache at {}", dir.display()),
        None => {
            println!("No local tldr cache found. Checked:");
            for d in crate::tldr::candidate_dirs() {
                println!("  {}", d.display());
            }
            println!("\nInstall tealdeer (`cargo install tealdeer` or your package manager) and run `tldr --update` first.");
        }
    }
    Ok(())
}

pub fn tldr_sync(db: &mut Database, platforms: &[String], only_tool: Option<&str>, dry_run: bool) -> Result<()> {
    let Some(dir) = crate::tldr::find_cache() else {
        bail!("no local tldr cache found — run `recall tldr status` to see where recall looked");
    };
    if dry_run {
        eprintln!("(dry run: nothing will be written)");
    }
    let plats = if platforms.is_empty() {
        crate::tldr::DEFAULT_PLATFORMS.iter().map(|s| s.to_string()).collect()
    } else {
        platforms.to_vec()
    };
    if dry_run {
        // sync against a throwaway copy so a dry run truly writes nothing
        let mut tmp = Database::new(&std::env::temp_dir().join(format!("recall-tldr-dryrun-{}.db", std::process::id())))?;
        tmp.sync_pack(&[])?; // no-op, just exercising the same path for parity
        let s = crate::tldr::ingest(&mut tmp, &dir, &plats, only_tool)?;
        report_tldr(&s);
        let _ = std::fs::remove_file(tmp.path.clone());
        return Ok(());
    }
    let s = crate::tldr::ingest(db, &dir, &plats, only_tool)?;
    db.meta_set("tldr_synced_at", &util::now_str())?;
    report_tldr(&s);
    Ok(())
}

fn report_tldr(s: &crate::tldr::TldrStats) {
    println!("tldr sync: {} pages scanned, {} candidates", s.pages, s.candidates);
    println!("  added             {}", s.added);
    println!("  already covered   {} (a command you or the pack already has)", s.already_covered);
    println!("  duplicate         {}", s.duplicates);
}

// ─── add (extended) ─────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
pub fn add(
    db: &Database,
    title: &str,
    content: &str,
    category: &str,
    tags: &str,
    tool: Option<&str>,
    command: Option<&str>,
    keywords: &str,
    danger_flag: bool,
) -> Result<()> {
    let cat = Category::from_str(category);
    let tag_vec: Vec<String> = tags.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let cmd_text = command.map(|s| s.to_string()).unwrap_or_else(|| {
        if cat == Category::Command {
            crate::models::fenced_blocks(content).into_iter().find(|b| !b.trim().is_empty()).unwrap_or_default()
        } else {
            String::new()
        }
    });
    let tool_text = tool.map(|s| s.to_lowercase()).unwrap_or_else(|| crate::derive::derive_tool(title, content, &tag_vec));
    let danger = danger_flag || crate::danger::danger_reason(&cmd_text).is_some();
    let id = db.add_new(&NewEntry {
        title: title.to_string(),
        content: content.to_string(),
        category: cat,
        tags: tag_vec,
        tool: tool_text,
        command: cmd_text,
        keywords: keywords.to_string(),
        danger,
        source: Source::User,
        pack_key: String::new(),
    })?;
    println!("Added #{}: {}", id, title);
    Ok(())
}

// ─── shell integration ────────────────────────────────────────────────────────

pub fn init_shell(shell: &str) -> Result<String> {
    Ok(match shell {
        "zsh" => include_str!("shell/init.zsh").to_string(),
        "bash" => include_str!("shell/init.bash").to_string(),
        "fish" => include_str!("shell/init.fish").to_string(),
        other => bail!("unknown shell '{}' (zsh, bash, fish)", other),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Category;

    fn tmp_db() -> (Database, std::path::PathBuf) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let mut p = std::env::temp_dir();
        p.push(format!("recall_cmdstest_{}_{}.db", std::process::id(), nanos));
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
    fn load_ctx_treats_pinned_and_remembered_correctly() {
        let (db, p) = tmp_db();
        db.set_var("target", "10.0.0.5").unwrap();
        db.remember_var("file", "a.txt").unwrap();
        let ctx = load_ctx(&db).unwrap();
        assert_eq!(ctx.pinned.get("target").map(String::as_str), Some("10.0.0.5"));
        assert!(!ctx.pinned.contains_key("file"));
        assert_eq!(ctx.last.get("file").map(String::as_str), Some("a.txt"));
        cleanup(&p);
    }

    #[test]
    fn resolve_quiet_never_blocks_and_reports_the_rest() {
        let ctx = fill::Ctx::default();
        let (text, unresolved) = resolve_quiet("nmap {{target}} -p {{port:80}}", &ctx);
        assert_eq!(text, "nmap {{target}} -p 80");
        assert_eq!(unresolved, vec!["target"]);
    }

    #[test]
    fn best_match_reports_the_runner_up_count() {
        let (db, p) = tmp_db();
        db.add_new(&NewEntry { title: "A".into(), tool: "nmap".into(), command: "nmap a".into(), category: Category::Command, ..Default::default() }).unwrap();
        db.add_new(&NewEntry { title: "B".into(), tool: "nmap".into(), command: "nmap b".into(), category: Category::Command, ..Default::default() }).unwrap();
        let (e, others) = best_match(&db, &["nmap".to_string()]).unwrap().unwrap();
        assert_eq!(e.tool, "nmap");
        assert_eq!(others, 1);
        assert!(best_match(&db, &["zzzznothing".to_string()]).unwrap().is_none());
        cleanup(&p);
    }

    #[test]
    fn highlight_bolds_matches_case_insensitively_without_nested_resets() {
        let p = Paint { on: true };
        let out = highlight("Full TCP Sweep", &["tcp".to_string()], p);
        assert_eq!(out, "Full \u{1b}[1mTCP\u{1b}[0m Sweep", "each match is its own open/reset pair");
        assert_eq!(highlight("no match here", &["zzz".to_string()], p), "no match here");
        assert_eq!(highlight("Full TCP Sweep", &["tcp".to_string()], Paint { on: false }), "Full TCP Sweep");
    }

    #[test]
    fn write_entry_shows_command_tags_and_danger_reason() {
        let e = Entry {
            id: 1, title: "Wipe".into(), content: String::new(), category: Category::Command,
            tags: vec!["disk".into()], favorite: true, created_at: String::new(), updated_at: String::new(),
            tool: "dd".into(), command: "dd if=/dev/zero of=/dev/sda".into(), keywords: String::new(),
            danger: true, uses: 3, last_used: String::new(), source: Source::User, pack_key: String::new(), pack_hash: String::new(),
        };
        let mut buf = Vec::new();
        write_entry(&mut buf, &e, Paint { on: false }, &[]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("dd › Wipe"));
        assert!(s.contains("dd if=/dev/zero of=/dev/sda"));
        assert!(s.contains("tags: disk"));
        assert!(s.contains("used 3×"));
        assert!(s.contains("writes raw data to a device"));
    }

    #[test]
    fn add_derives_tool_command_and_danger_when_not_given() {
        let (db, p) = tmp_db();
        add(&db, "Wipe a disk", "```bash\ndd if=/dev/zero of=/dev/sdb\n```", "command", "disk", None, None, "", false).unwrap();
        let e = db.get_all_entries().unwrap().into_iter().next().unwrap();
        assert_eq!(e.tool, "dd");
        assert_eq!(e.command, "dd if=/dev/zero of=/dev/sdb");
        assert!(e.danger, "auto-detected from the derived command");
        cleanup(&p);
    }

    #[test]
    fn human_bytes_formats_reasonably() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(6_500_000), "6.2 MB");
    }

    #[test]
    fn init_shell_covers_every_supported_shell() {
        for s in ["zsh", "bash", "fish"] {
            let out = init_shell(s).unwrap();
            assert!(out.contains("recall"), "{} snippet should mention recall", s);
        }
        assert!(init_shell("csh").is_err());
    }
}
