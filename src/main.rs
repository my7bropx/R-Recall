mod app;
mod clipboard;
mod cmds;
mod danger;
mod db;
mod dedupe;
mod derive;
mod fill;
mod import;
mod mdscan;
mod models;
mod pack;
mod pairs;
mod pick;
mod query;
mod theme;
mod tldr;
mod ui;
mod util;

use std::{io, path::PathBuf};

use anyhow::Result;
use clap::{Parser, Subcommand};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::{App, CmdOutcome, FormField, Mode, Screen};
use db::Database;

const VIEW_JUMP: u16 = 10; // half-page scroll in full view

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "recall", about = "Terminal knowledge base with Vim keys and fuzzy search")]
struct Cli {
    /// Color theme for the TUI (safelight, default, gruvbox, catppuccin, nord, tokyonight)
    #[arg(long, global = true)]
    theme: Option<String>,

    /// Force 24-bit color on or off (default: auto-detect via COLORTERM)
    #[arg(long, global = true)]
    truecolor: Option<bool>,

    /// Paint the theme background, or inherit the terminal's (default: per theme)
    #[arg(long, global = true)]
    background: Option<bool>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Shorthand for `recall show <words>` — `recall nmap ping sweep`
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    words: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Add an entry directly from the command line
    Add {
        #[arg(short, long)]
        title: String,
        #[arg(short, long, default_value = "")]
        content: String,
        #[arg(short = 'C', long, default_value = "note")]
        category: String,
        #[arg(long, default_value = "")]
        tags: String,
        /// The program this is about (nmap, git, ...) — inferred from content if omitted
        #[arg(long)]
        tool: Option<String>,
        /// The copy-ready command, if it's not just the first fenced block of --content
        #[arg(short = 'm', long)]
        command: Option<String>,
        /// Extra search words: synonyms, the way you'd ask for this in plain English
        #[arg(short, long, default_value = "")]
        keywords: String,
        /// Force the destructive-command warning even if not auto-detected
        #[arg(long)]
        danger: bool,
    },
    /// Quick search — legacy plain output, one line per hit (pipe-safe: | head works)
    Search {
        #[arg(allow_hyphen_values = true)]
        query: String,
    },
    /// Search and show full entries: tool, command, tags, danger warnings
    Show {
        // No `trailing_var_arg` here (unlike Cmd/Copy/Run): that would also
        // swallow a `--limit` typed after the query words. `allow_hyphen_values`
        // alone still lets flag-shaped words (-sV, -p-) through.
        #[arg(allow_hyphen_values = true)]
        query: Vec<String>,
        /// If the query itself has flag-shaped words (-sV), put --limit
        /// first: `recall show --limit 3 nmap -sV`
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Print just the resolved command for the best match. Never prompts —
    /// safe inside `$(recall cmd ...)`.
    Cmd {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        query: Vec<String>,
    },
    /// Resolve placeholders (prompting for anything not covered by a pinned
    /// variable) and copy the command to the clipboard
    Copy {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        query: Vec<String>,
    },
    /// Resolve, prompt, confirm, and run the command in your shell
    Run {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        query: Vec<String>,
    },
    /// Interactively fuzzy-pick a command and print it resolved (what the
    /// shell widget from `recall init` binds to Ctrl-G)
    Pick {
        #[arg(allow_hyphen_values = true)]
        query: Option<String>,
    },
    /// Pin a variable: matching placeholders fill silently from now on
    Set { name: String, value: Vec<String> },
    /// Remove a pinned or remembered variable
    Unset { name: String },
    /// List pinned and remembered variables
    Vars,
    /// List tools by how many entries cover them
    Tools {
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// List tags by frequency
    Tags {
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Database summary: counts, sources, coverage
    Stats,
    /// Every entry as one JSON object per line (JSONL) on stdout — a
    /// portable backup you can grep or pipe through `jq`
    Export,
    /// Copy the database file somewhere safe (defaults next to the original,
    /// timestamped)
    Backup { path: Option<PathBuf> },
    /// Check database / search-index health, optionally repairing it
    Doctor {
        #[arg(long)]
        fix: bool,
    },
    /// Print the path to the database file
    DbPath,
    /// Find and remove duplicate entries (reports only unless --apply is given)
    Dedupe {
        /// What counts as a duplicate: exact | title | normalized
        #[arg(long, default_value = "exact")]
        key: String,
        /// Which copy to keep: first | last | longest
        #[arg(long, default_value = "longest")]
        keep: String,
        /// Actually delete. Without this, nothing is written.
        #[arg(long)]
        apply: bool,
        /// Do not merge tags / favorites from the removed copies into the kept one
        #[arg(long)]
        no_merge: bool,
        /// How many example groups to print
        #[arg(long, default_value_t = 5)]
        show: usize,
    },
    /// Import entries from a Markdown/notes file (every heading becomes an entry;
    /// code fences, page headers and converter debris are handled — see the README)
    Import {
        /// Path to the Markdown file
        path: PathBuf,
        /// Parse and report what the importer did, without writing to the database
        #[arg(long)]
        dry_run: bool,
        /// Only import entries flagged CRITICAL or IMPORTANT
        #[arg(long)]
        flagged_only: bool,
        /// Extra tag to attach to every imported entry (repeatable)
        #[arg(long = "tag")]
        tag: Vec<String>,
        /// Also store each annotated command (`# what it does` above it, a
        /// `- `cmd` - description` bullet, a table row, a key table) as its own
        /// entry, so `recall cmd` prints one command instead of a whole section
        #[arg(long)]
        per_command: bool,
    },
    /// The built-in knowledge pack (curated tool/command reference)
    Pack {
        #[command(subcommand)]
        action: PackCmd,
    },
    /// Ingest example commands from a local tldr-pages cache. Opt-in — never
    /// runs on its own, since it can add thousands of generic entries.
    Tldr {
        #[command(subcommand)]
        action: TldrCmd,
    },
    /// Print a shell integration snippet: `eval "$(recall init zsh)"`
    Init { shell: String },
}

#[derive(Subcommand)]
enum PackCmd {
    /// Add/update built-in entries. Never overwrites one you've edited, and
    /// never re-adds one you deleted.
    Sync {
        #[arg(long)]
        dry_run: bool,
    },
    /// Forget which built-in entries you deleted, so the next sync restores them
    Restore,
}

#[derive(Subcommand)]
enum TldrCmd {
    /// Show whether a local tldr-pages cache was found, and where recall looked
    Status,
    /// Import example commands from it (skips anything already covered)
    Sync {
        /// Platform pages to read (repeatable). Default: common, linux.
        #[arg(long = "platform")]
        platform: Vec<String>,
        /// Import just this one tool's page
        #[arg(long)]
        tool: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
}

// ─── Entry point ─────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Resolve the theme up front so a bad --theme is an error on every path,
    // not just when launching the TUI.
    let mut active_theme = theme::Theme::from_env();
    if let Some(ref name) = cli.theme {
        match theme::Theme::by_name(name) {
            Some(found) => active_theme = found.with_color_mode(active_theme.rgb),
            None => {
                eprintln!("Unknown theme '{}' — available: {}", name, theme::Theme::names());
                std::process::exit(2);
            }
        }
    }
    if let Some(tc) = cli.truecolor {
        active_theme = active_theme.with_color_mode(tc);
    }
    if let Some(bgp) = cli.background {
        active_theme = active_theme.with_background(bgp);
    }

    let db_path = recall_db_path();
    let mut db  = Database::new(&db_path)?;
    if let Some(note) = &db.migration_note {
        eprintln!("{}", note);
    }
    let paint = util::Paint::for_stdout();

    match cli.command {
        Some(Commands::Add { title, content, category, tags, tool, command, keywords, danger }) => {
            cmds::add(&db, &title, &content, &category, &tags, tool.as_deref(), command.as_deref(), &keywords, danger)?;
        }

        Some(Commands::Search { query }) => cmds::search(&db, &query)?,
        Some(Commands::Show { query, limit }) => cmds::show(&db, &query, limit, paint)?,
        Some(Commands::Cmd { query }) => cmds::cmd(&db, &query)?,
        Some(Commands::Copy { query }) => cmds::copy(&db, &query)?,
        Some(Commands::Run { query }) => cmds::run(&db, &query)?,
        Some(Commands::Pick { query }) => match pick::run(&db, &active_theme, query.as_deref())? {
            Some(text) => println!("{}", text),
            None => std::process::exit(1),
        },
        Some(Commands::Set { name, value }) => cmds::set_var(&db, &name, &value.join(" "))?,
        Some(Commands::Unset { name }) => cmds::unset_var(&db, &name)?,
        Some(Commands::Vars) => cmds::list_vars(&db, paint)?,
        Some(Commands::Tools { limit }) => cmds::tools(&db, limit)?,
        Some(Commands::Tags { limit }) => cmds::tags(&db, limit)?,
        Some(Commands::Stats) => cmds::stats(&db, paint)?,
        Some(Commands::Export) => cmds::export(&db)?,
        Some(Commands::Backup { path }) => cmds::backup(&db, path.as_deref())?,
        Some(Commands::Doctor { fix }) => cmds::doctor(&mut db, fix, paint)?,

        Some(Commands::DbPath) => {
            println!("{}", db_path.display());
        }

        Some(Commands::Pack { action }) => match action {
            PackCmd::Sync { dry_run } => cmds::pack_sync(&mut db, dry_run, paint)?,
            PackCmd::Restore => cmds::pack_restore(&db)?,
        },
        Some(Commands::Tldr { action }) => match action {
            TldrCmd::Status => cmds::tldr_status()?,
            TldrCmd::Sync { platform, tool, dry_run } => cmds::tldr_sync(&mut db, &platform, tool.as_deref(), dry_run)?,
        },
        Some(Commands::Init { shell }) => print!("{}", cmds::init_shell(&shell)?),

        Some(Commands::Dedupe { key, keep, apply, no_merge, show }) => {
            let dup_key = match dedupe::DupKey::parse(&key) {
                Some(k) => k,
                None => {
                    eprintln!("Unknown --key '{}' (exact | title | normalized)", key);
                    std::process::exit(2);
                }
            };
            let keep_rule = match dedupe::KeepRule::parse(&keep) {
                Some(k) => k,
                None => {
                    eprintln!("Unknown --keep '{}' (first | last | longest)", keep);
                    std::process::exit(2);
                }
            };
            let merge   = !no_merge;
            let entries = db.get_all_entries()?;
            let plan    = dedupe::plan(&entries, dup_key, keep_rule, merge);
            let dupes   = dedupe::removable(&plan);

            if plan.is_empty() {
                println!(
                    "No duplicates found among {} entries (key: {}).",
                    entries.len(),
                    dup_key.label()
                );
            } else {
                println!(
                    "{} duplicate groups · {} entries removable · {} would remain  (key: {}, keep: {})",
                    plan.len(),
                    dupes,
                    entries.len() - dupes,
                    dup_key.label(),
                    keep_rule.label()
                );
                for g in plan.iter().take(show) {
                    println!(
                        "  {:<58} keep id {:<6} remove {:?}",
                        util::truncate(&g.title, 58),
                        g.survivor,
                        g.victims
                    );
                }
                if plan.len() > show {
                    println!("  … and {} more groups", plan.len() - show);
                }

                if apply {
                    let (removed, merged) = db.apply_dedupe(&plan)?;
                    println!(
                        "\nRemoved {} duplicate entries{}.",
                        removed,
                        if merged > 0 {
                            format!(", merged tags/favorites into {} kept entries", merged)
                        } else {
                            String::new()
                        }
                    );
                } else {
                    println!("\nNothing was written. Re-run with --apply to delete.");
                }
            }
        }

        Some(Commands::Import { path, dry_run, flagged_only, tag, per_command }) => {
            let opts = import::ImportOptions { dry_run, flagged_only, extra_tags: tag, per_command };
            let stats = import::import_markdown(&mut db, &path, &opts)?;
            let verb = if dry_run { "Would import" } else { "Imported" };
            println!(
                "{} {} entries ({} skipped, {} duplicates) from {}",
                verb, stats.candidates, stats.skipped, stats.duplicates, path.display()
            );
            println!("{}", stats.report.render());
        }

        None => {
            if cli.words.is_empty() {
                run_tui(db, active_theme)?;
            } else {
                cmds::show(&db, &cli.words, 10, paint)?;
            }
        }
    }

    Ok(())
}


fn recall_db_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let dir  = PathBuf::from(home).join(".local").join("share").join("recall");
    std::fs::create_dir_all(&dir).ok();
    dir.join("recall.db")
}

// ─── TUI event loop ───────────────────────────────────────────────────────────

enum Loop {
    Continue,
    Quit,
    Editor,
}

fn run_tui(db: Database, theme: theme::Theme) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend  = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;
    let mut app  = App::new(db)?;
    app.theme = theme;

    loop {
        term.draw(|f| ui::render(f, &mut app))?;

        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                // Only act on key *press* (Kitty/modern terminals also emit Release/Repeat)
                if key.kind == KeyEventKind::Release {
                    continue;
                }
                match handle_key(&mut app, key)? {
                    Loop::Quit => break,
                    Loop::Editor => open_external_editor(&mut term, &mut app)?,
                    Loop::Continue => {}
                }
            }
        }
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn open_external_editor(
    term: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;

    let editor   = std::env::var("EDITOR").unwrap_or_else(|_| "vim".to_string());
    let tmp_path = std::env::temp_dir().join("recall_content_edit.tmp");
    std::fs::write(&tmp_path, &app.form.content).ok();
    std::process::Command::new(&editor).arg(&tmp_path).status().ok();
    if let Ok(text) = std::fs::read_to_string(&tmp_path) {
        app.form.content = text;
    }
    std::fs::remove_file(&tmp_path).ok();

    enable_raw_mode()?;
    execute!(term.backend_mut(), EnterAlternateScreen)?;
    term.clear()?;
    Ok(())
}

// ─── key dispatch: mode first, then screen ─────────────────────────────────────

fn handle_key(app: &mut App, key: event::KeyEvent) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    match app.mode {
        Mode::Command => return handle_cmdline(app, key, false),
        Mode::Search  => return handle_cmdline(app, key, true),
        Mode::Insert  => return handle_insert(app, key),
        Mode::Visual | Mode::VisualLine => return handle_view_visual(app, key),
        Mode::Normal  => {}
    }

    // Help overlay swallows keys while open.
    if app.help_open {
        if matches!(key.code, KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc) {
            app.help_open = false;
        }
        return Ok(Loop::Continue);
    }

    if ctrl && key.code == KeyCode::Char('c') {
        return Ok(Loop::Quit);
    }

    match app.screen {
        Screen::List => handle_list_normal(app, key),
        Screen::View => handle_view_normal(app, key),
        Screen::Form => handle_form_normal(app, key),
    }
}

// ── Command / Search line ──────────────────────────────────────────────────────

fn handle_cmdline(app: &mut App, key: event::KeyEvent, is_search: bool) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Enter => {
            if is_search {
                app.search_confirm();
                return Ok(Loop::Continue);
            }
            let cmd = std::mem::take(&mut app.cmdline);
            app.mode = Mode::Normal;
            return match app.execute_command(&cmd)? {
                CmdOutcome::Quit   => Ok(Loop::Quit),
                CmdOutcome::Editor => Ok(Loop::Editor),
                CmdOutcome::None   => Ok(Loop::Continue),
            };
        }
        KeyCode::Esc => {
            if is_search {
                app.search_cancel();
            } else {
                app.mode = Mode::Normal;
                app.cmdline.clear();
            }
        }
        KeyCode::Char('c') if ctrl => {
            if is_search {
                app.search_cancel();
            } else {
                app.mode = Mode::Normal;
                app.cmdline.clear();
            }
        }
        KeyCode::Backspace => {
            if is_search {
                app.search_pop();
            } else {
                app.cmd_pop();
            }
        }
        KeyCode::Char(c) if !ctrl => {
            if is_search {
                app.search_push(c);
            } else {
                app.cmd_push(c);
            }
        }
        _ => {}
    }
    Ok(Loop::Continue)
}

// ── Insert mode (form fields) ──────────────────────────────────────────────────

fn handle_insert(app: &mut App, key: event::KeyEvent) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Esc => app.mode = Mode::Normal,
        KeyCode::Char('c') if ctrl => app.mode = Mode::Normal,
        KeyCode::Enter => match app.form.focused {
            FormField::Content => app.form.content.push('\n'),
            _ => app.form.focused = app.form.focused.next(),
        },
        KeyCode::Tab     => app.form.focused = app.form.focused.next(),
        KeyCode::BackTab => app.form.focused = app.form.focused.prev(),
        KeyCode::Backspace => match app.form.focused {
            FormField::Title    => { app.form.title.pop(); }
            FormField::Tags     => { app.form.tags.pop(); }
            FormField::Content  => { app.form.content.pop(); }
            FormField::Category => {}
        },
        KeyCode::Char(c) if !ctrl => match app.form.focused {
            FormField::Title    => app.form.title.push(c),
            FormField::Tags     => app.form.tags.push(c),
            FormField::Content  => app.form.content.push(c),
            FormField::Category => {}
        },
        _ => {}
    }
    Ok(Loop::Continue)
}

// ── Normal mode — list ─────────────────────────────────────────────────────────

fn handle_list_normal(app: &mut App, key: event::KeyEvent) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if let Some(op) = app.pending {
        app.pending = None;
        let done = match (op, key.code) {
            ('g', KeyCode::Char('g')) => { app.to_top(); true }
            ('g', KeyCode::Char('t')) => { app.next_tab(); true }
            ('g', KeyCode::Char('T')) => { app.prev_tab(); true }
            ('d', KeyCode::Char('d')) => { app.delete_selected()?; true }
            ('y', KeyCode::Char('y')) => { app.yank_selected(); true }
            ('+', KeyCode::Char('y')) => { app.yank_selected(); true }
            _ => false,
        };
        if done {
            return Ok(Loop::Continue);
        }
    }

    app.status = None;
    match key.code {
        KeyCode::Char('c') if ctrl => return Ok(Loop::Quit),
        KeyCode::Char('d') if ctrl => app.half_down(),
        KeyCode::Char('u') if ctrl => app.half_up(),
        KeyCode::Char('r') if ctrl => app.redo()?,
        KeyCode::Char('j') => app.move_down(),
        KeyCode::Char('k') => app.move_up(),
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => app.arrow_hint(),
        KeyCode::Char('g') => app.pending = Some('g'),
        KeyCode::Char('G') => app.to_bottom(),
        KeyCode::Char('d') => app.pending = Some('d'),
        KeyCode::Char('y') => app.pending = Some('y'),
        KeyCode::Char('Y') => app.yank_command_selected(),
        KeyCode::Char('+') => app.pending = Some('+'),
        KeyCode::Char('l') | KeyCode::Enter => app.open_view(),
        KeyCode::Char('v') => {
            if app.selected_entry().is_some() {
                app.view_top(); // fresh cursor at the top of the preview pane
                app.enter_visual();
            }
        }
        KeyCode::Char('V') => {
            if app.selected_entry().is_some() {
                app.view_top();
                app.enter_visual_line();
            }
        }
        KeyCode::Char('o') => app.new_form(),
        KeyCode::Char('e') => app.edit_form(),
        KeyCode::Char('f') => app.toggle_favorite_selected()?,
        KeyCode::Char('u') => app.undo()?,
        KeyCode::Char('n') => app.next_match(),
        KeyCode::Char('N') => app.prev_match(),
        KeyCode::Char('/') => app.start_search(),
        KeyCode::Char(':') => app.start_command(),
        KeyCode::Char('?') => app.help_open = true,
        KeyCode::Char('q') => return Ok(Loop::Quit),
        KeyCode::Esc       => app.clear_search_filter(),
        KeyCode::PageDown  => app.half_down(),
        KeyCode::PageUp    => app.half_up(),
        _ => {}
    }
    Ok(Loop::Continue)
}

// ── Normal mode — full view ─────────────────────────────────────────────────────

fn handle_view_normal(app: &mut App, key: event::KeyEvent) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if let Some(op) = app.pending {
        app.pending = None;
        let done = match (op, key.code) {
            ('g', KeyCode::Char('g')) => { app.view_top(); true }
            ('y', KeyCode::Char('y')) => { app.yank_current_line(); true }
            ('+', KeyCode::Char('y')) => { app.yank_current_line(); true }
            _ => false,
        };
        if done {
            return Ok(Loop::Continue);
        }
    }

    match key.code {
        KeyCode::Char('c') if ctrl => return Ok(Loop::Quit),
        KeyCode::Char('d') if ctrl => app.view_down(VIEW_JUMP),
        KeyCode::Char('u') if ctrl => app.view_up(VIEW_JUMP),
        KeyCode::Char('j') => app.view_down(1),
        KeyCode::Char('k') => app.view_up(1),
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => app.arrow_hint(),
        KeyCode::Char('g') => app.pending = Some('g'),
        KeyCode::Char('G') => app.view_bottom(),
        KeyCode::Char('y') => app.pending = Some('y'),
        KeyCode::Char('+') => app.pending = Some('+'),
        KeyCode::Char('v') => app.enter_visual(),
        KeyCode::Char('V') => app.enter_visual_line(),
        KeyCode::Char('i') => {
            app.edit_form();
            app.form.focused = FormField::Content;
        }
        KeyCode::Char('f') => app.toggle_favorite_selected()?,
        KeyCode::Char('q') | KeyCode::Esc | KeyCode::Char('h') | KeyCode::Enter => {
            app.screen = Screen::List;
        }
        KeyCode::Char('/') => { app.screen = Screen::List; app.start_search(); }
        KeyCode::Char(':') => app.start_command(),
        KeyCode::Char('?') => app.help_open = true,
        KeyCode::PageDown  => app.view_down(VIEW_JUMP),
        KeyCode::PageUp    => app.view_up(VIEW_JUMP),
        _ => {}
    }
    Ok(Loop::Continue)
}

// ── Visual / Visual-Line — full view (entered with v / V) ───────────────────
//
// j/k/gg/G move the cursor and extend the selection (autoscrolling to keep it
// in view); h/l move within a line for char-wise Visual. `y` and the Vim-style
// `+y` (yank to the system-clipboard register) both copy exactly the
// highlighted text — no extra lines or trailing spaces — then return to
// Normal mode.
fn handle_view_visual(app: &mut App, key: event::KeyEvent) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

    if let Some(op) = app.pending {
        app.pending = None;
        let done = match (op, key.code) {
            ('g', KeyCode::Char('g')) => { app.view_top(); true }
            ('+', KeyCode::Char('y')) => { app.yank_visual(); true }
            _ => false,
        };
        if done {
            return Ok(Loop::Continue);
        }
    }

    match key.code {
        KeyCode::Char('c') if ctrl => app.exit_visual(),
        KeyCode::Esc => app.exit_visual(),
        KeyCode::Char('v') => {
            if app.mode == Mode::Visual { app.exit_visual(); } else { app.mode = Mode::Visual; }
        }
        KeyCode::Char('V') => {
            if app.mode == Mode::VisualLine { app.exit_visual(); } else { app.mode = Mode::VisualLine; }
        }
        KeyCode::Char('d') if ctrl => app.view_down(VIEW_JUMP),
        KeyCode::Char('u') if ctrl => app.view_up(VIEW_JUMP),
        KeyCode::Char('j') | KeyCode::Down => app.view_down(1),
        KeyCode::Char('k') | KeyCode::Up   => app.view_up(1),
        KeyCode::Char('h') | KeyCode::Left  => app.cursor_left(),
        KeyCode::Char('l') | KeyCode::Right => app.cursor_right(),
        KeyCode::Char('g') => app.pending = Some('g'),
        KeyCode::Char('G') => app.view_bottom(),
        KeyCode::Char('y') => app.yank_visual(),
        KeyCode::Char('+') => app.pending = Some('+'),
        _ => {}
    }
    Ok(Loop::Continue)
}

// ── Normal mode — form ───────────────────────────────────────────────────────

fn handle_form_normal(app: &mut App, key: event::KeyEvent) -> Result<Loop> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        // In a form, Ctrl-C behaves like Esc (Vim-style) rather than quitting.
        KeyCode::Char('c') if ctrl => { app.try_cancel_form(false); }
        KeyCode::Char('i') | KeyCode::Char('a') => app.mode = Mode::Insert,
        KeyCode::Char('j') | KeyCode::Tab => {
            app.form.focused = app.form.focused.next()
        }
        KeyCode::Char('k') | KeyCode::BackTab => {
            app.form.focused = app.form.focused.prev()
        }
        KeyCode::Char('h') => {
            if app.form.focused == FormField::Category {
                app.form.category = app.form.category.cycle_prev();
            }
        }
        KeyCode::Char('l') => {
            if app.form.focused == FormField::Category {
                app.form.category = app.form.category.cycle_next();
            }
        }
        KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right => app.arrow_hint(),
        KeyCode::Char(':') => app.start_command(),
        KeyCode::Char('?') => app.help_open = true,
        KeyCode::Esc => { app.try_cancel_form(false); }
        _ => {}
    }
    Ok(Loop::Continue)
}
