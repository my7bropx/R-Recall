//! `recall pick` — a small fuzzy-pick UI for the shell widget (`recall init
//! zsh|bash|fish`) and for interactive use on its own.
//!
//! The UI paints to **stderr** and reads keys from stdin; only the final
//! resolved command is printed to **stdout**. That split matters: a shell
//! widget captures this program's output with `$(recall pick ...)`, which
//! redirects stdout only — stdin stays the live terminal, and stderr still
//! reaches the screen, so the picker renders normally while the capture
//! stays clean. Run directly in a terminal (not captured), stderr and stdout
//! land in the same place, so it looks exactly like any other TUI screen.

use std::io;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame, Terminal,
};

use crate::{cmds, db::Database, models::Entry, query::Query, theme::Theme, util::truncate};

const MAX_RESULTS: usize = 200;

fn search(db: &Database, query: &str) -> Result<Vec<Entry>> {
    let out = db.search(&Query::parse(query), MAX_RESULTS)?;
    db.get_entries(&out.hits.into_iter().map(|h| h.id).collect::<Vec<_>>())
}

/// Run the picker. Returns the resolved command, or `None` if cancelled or
/// the chosen entry has nothing to hand back.
pub fn run(db: &Database, theme: &Theme, initial: Option<&str>) -> Result<Option<String>> {
    enable_raw_mode()?;
    execute!(io::stderr(), EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(io::stderr()))?;

    let mut query = initial.unwrap_or("").to_string();
    let mut selected = 0usize;
    let mut hits = search(db, &query)?;
    let mut chosen: Option<i64> = None;

    loop {
        term.draw(|f| draw(f, theme, &query, &hits, selected))?;
        let Event::Key(key) = event::read()? else { continue };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => break,
            KeyCode::Char('c') if ctrl => break,
            KeyCode::Enter => {
                chosen = hits.get(selected).map(|e| e.id);
                break;
            }
            KeyCode::Up => selected = selected.saturating_sub(1),
            KeyCode::Char('p') if ctrl => selected = selected.saturating_sub(1),
            KeyCode::Down => selected = (selected + 1).min(hits.len().saturating_sub(1)),
            KeyCode::Char('n') if ctrl => selected = (selected + 1).min(hits.len().saturating_sub(1)),
            KeyCode::Backspace => {
                query.pop();
                selected = 0;
                hits = search(db, &query)?;
            }
            KeyCode::Char('u') if ctrl => {
                query.clear();
                selected = 0;
                hits = search(db, &query)?;
            }
            KeyCode::Char(c) if !ctrl => {
                query.push(c);
                selected = 0;
                hits = search(db, &query)?;
            }
            _ => {}
        }
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;

    let Some(id) = chosen else { return Ok(None) };
    let Some(entry) = db.get_entry(id)? else { return Ok(None) };
    if !entry.has_command() {
        eprintln!("'{}' has no command to insert.", entry.title);
        return Ok(None);
    }
    let ctx = cmds::load_ctx(db)?;
    let text = cmds::resolve_interactive(db, &entry.primary_command(), &ctx, true)?;
    db.record_use(id)?;
    Ok(Some(text))
}

fn draw(f: &mut Frame, th: &Theme, query: &str, hits: &[Entry], selected: usize) {
    let area = f.area();
    if th.paint_bg {
        f.render_widget(Block::default().style(Style::default().bg(th.bg_c()).fg(th.fg_c())), area);
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let input = Paragraph::new(format!("> {}", query)).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(th.accent_c()))
            .title(Span::styled(" recall — type to filter · Enter picks · Esc cancels ", Style::default().fg(th.fg_c()))),
    );
    f.render_widget(input, rows[0]);

    let width = rows[1].width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = hits
        .iter()
        .map(|e| {
            let head = if e.tool.is_empty() { e.title.clone() } else { format!("{} › {}", e.tool, e.title) };
            let danger = if e.danger { "  ⚠" } else { "" };
            let mut lines = vec![Line::from(Span::styled(
                format!("{}{}", head, danger),
                Style::default().fg(if e.danger { th.warn_c() } else { th.fg_c() }).add_modifier(Modifier::BOLD),
            ))];
            if e.has_command() {
                lines.push(Line::from(Span::styled(
                    format!("  {}", truncate(&e.primary_command(), width)),
                    Style::default().fg(th.dim_c()),
                )));
            }
            ListItem::new(lines)
        })
        .collect();

    let block = Block::default().borders(Borders::ALL).border_style(Style::default().fg(th.border_c()));
    let mut state = ListState::default();
    if !hits.is_empty() {
        state.select(Some(selected));
    }
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(th.sel_c()).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, rows[1], &mut state);

    let status = if hits.is_empty() {
        "no matches".to_string()
    } else {
        format!("{} match{}", hits.len(), if hits.len() == 1 { "" } else { "es" })
    };
    f.render_widget(Paragraph::new(status).style(Style::default().fg(th.dim_c())), rows[2]);
}
