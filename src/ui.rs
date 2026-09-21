use std::collections::HashSet;

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use crate::{
    app::{App, FormField, Mode, Screen},
    models::Category,
    theme::Theme,
};

fn cat_color(th: &Theme, cat: &Category) -> Color {
    match cat {
        Category::Command => th.cmd_c(),
        Category::Note    => th.note_c(),
        Category::Tool    => th.tool_c(),
    }
}

// ─── markdown / code highlighting ────────────────────────────────────────────

/// Split a line of prose into styled spans, honouring `` `code` ``, `**bold**`,
/// `*italic*` and `[link](url)`.
fn inline_spans(th: &Theme, s: &str) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf   = String::new();
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;

    let flush = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(
                std::mem::take(buf),
                Style::default().fg(th.fg_c()),
            ));
        }
    };

    while i < chars.len() {
        let c = chars[i];

        // `inline code`
        if c == '`' {
            flush(&mut buf, &mut spans);
            let mut code = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '`' {
                code.push(chars[i]);
                i += 1;
            }
            i += 1; // closing backtick
            spans.push(Span::styled(code, Style::default().fg(th.code_c())));
            continue;
        }

        // **bold**
        if c == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            flush(&mut buf, &mut spans);
            i += 2;
            let mut bold = String::new();
            while i < chars.len() {
                if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
                    i += 2;
                    break;
                }
                bold.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(
                bold,
                Style::default().fg(th.bold_c()).add_modifier(Modifier::BOLD),
            ));
            continue;
        }

        // *italic* / _italic_
        if (c == '*' || c == '_') && i + 1 < chars.len() && chars[i + 1] != ' ' {
            let close = c;
            if let Some(rel) = chars[i + 1..].iter().position(|&x| x == close) {
                let text: String = chars[i + 1..i + 1 + rel].iter().collect();
                if !text.contains(' ') || text.len() < 40 {
                    flush(&mut buf, &mut spans);
                    spans.push(Span::styled(
                        text,
                        Style::default().fg(th.italic_c()).add_modifier(Modifier::ITALIC),
                    ));
                    i = i + rel + 2;
                    continue;
                }
            }
        }

        // [link text](destination)
        if c == '[' {
            if let Some(rel_close) = chars[i..].iter().position(|&x| x == ']') {
                let after = i + rel_close + 1;
                if after < chars.len() && chars[after] == '(' {
                    if let Some(rel_end) = chars[after..].iter().position(|&x| x == ')') {
                        let text: String = chars[i + 1..i + rel_close].iter().collect();
                        let url:  String = chars[after + 1..after + rel_end].iter().collect();
                        flush(&mut buf, &mut spans);
                        spans.push(Span::styled(
                            text,
                            Style::default().fg(th.link_c()).add_modifier(Modifier::UNDERLINED),
                        ));
                        spans.push(Span::styled(
                            format!(" ({})", url),
                            Style::default().fg(th.link_url_c()),
                        ));
                        i = after + rel_end + 1;
                        continue;
                    }
                }
            }
        }

        buf.push(c);
        i += 1;
    }
    flush(&mut buf, &mut spans);
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// Themed Markdown rendering. Heading levels get distinct colors, fenced code
/// bodies are separated from inline code, and `#` comments inside code blocks
/// are dimmed and italicised.
fn highlight_markdown(th: &Theme, content: &str) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_code = false;

    for raw in content.lines() {
        let trimmed = raw.trim_start();

        // fence markers, with the language tag dimmed
        if trimmed.starts_with("```") {
            in_code = !in_code;
            out.push(Line::from(Span::styled(
                raw.to_string(),
                Style::default().fg(th.dim_c()),
            )));
            continue;
        }

        if in_code {
            let style = if trimmed.starts_with('#') {
                Style::default().fg(th.comment_c()).add_modifier(Modifier::ITALIC)
            } else {
                Style::default().fg(th.code_block_c())
            };
            out.push(Line::from(Span::styled(raw.to_string(), style)));
            continue;
        }

        // headings: hashes dimmed, text colored by level
        if trimmed.starts_with('#') {
            let level = trimmed.chars().take_while(|&c| c == '#').count();
            let indent = &raw[..raw.len() - trimmed.len()];
            let hashes: String = trimmed.chars().take(level).collect();
            let rest: String = trimmed.chars().skip(level).collect();
            out.push(Line::from(vec![
                Span::raw(indent.to_string()),
                Span::styled(hashes, Style::default().fg(th.dim_c())),
                Span::styled(
                    rest,
                    Style::default().fg(th.head_c(level)).add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        }

        // blockquote
        if trimmed.starts_with("> ") {
            let indent = &raw[..raw.len() - trimmed.len()];
            let mut spans = vec![
                Span::raw(indent.to_string()),
                Span::styled("┃ ", Style::default().fg(th.dim_c())),
            ];
            spans.extend(inline_spans(th, &trimmed[2..]));
            out.push(Line::from(spans));
            continue;
        }

        // bullets
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            let indent = &raw[..raw.len() - trimmed.len()];
            let mut spans = vec![
                Span::raw(indent.to_string()),
                Span::styled("• ", Style::default().fg(th.accent_c())),
            ];
            spans.extend(inline_spans(th, &trimmed[2..]));
            out.push(Line::from(spans));
            continue;
        }

        out.push(Line::from(inline_spans(th, raw)));
    }
    out
}

// ─── root render ───────────────────────────────────────────────────────────────

pub fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Paint the app surface so the theme reads as itself, rather than
    // inheriting whatever background the terminal happens to have.
    if app.theme.paint_bg {
        f.render_widget(
            Block::default().style(Style::default().bg(app.theme.bg_c()).fg(app.theme.fg_c())),
            area,
        );
    }
    match app.screen {
        Screen::List => render_list_screen(f, app, area),
        Screen::Form => {
            render_list_screen(f, app, area);
            render_form(f, app, area);
        }
        Screen::View => render_view_screen(f, app, area),
    }
    if app.help_open {
        let th = app.theme;
        render_help(f, &th, area);
    }
}

fn render_list_screen(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    render_header(f, app, rows[0]);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(rows[1]);

    render_list(f, app, cols[0]);
    render_preview(f, app, cols[1]);
    render_statusline(f, app, rows[2]);
}

fn render_header(f: &mut Frame, app: &App, area: Rect) {
    let th      = &app.theme;
    let active  = Style::default().fg(th.bg_c()).bg(th.accent_c()).add_modifier(Modifier::BOLD);
    let passive = Style::default().fg(th.dim_c());

    let mut spans = vec![
        Span::styled(" RECALL ", Style::default().fg(th.accent_c()).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];
    let tabs = [
        ("ALL",  app.cat_filter.is_none()),
        ("CMD",  app.cat_filter == Some(Category::Command)),
        ("NOTE", app.cat_filter == Some(Category::Note)),
        ("TOOL", app.cat_filter == Some(Category::Tool)),
        ("FAV",  app.fav_filter),
    ];
    for (label, on) in tabs {
        let style = if on {
            if label == "FAV" {
                Style::default().fg(th.bg_c()).bg(th.star_c()).add_modifier(Modifier::BOLD)
            } else {
                active
            }
        } else {
            passive
        };
        spans.push(Span::styled(format!(" {} ", label), style));
        spans.push(Span::raw(" "));
    }
    spans.push(Span::styled(
        format!(" sort:{}{} ", app.sort_key.label(), if app.sort_rev { "↓" } else { "" }),
        Style::default().fg(th.dim_c()),
    ));
    spans.push(Span::styled(" gt/gT tabs ", Style::default().fg(th.dim_c())));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_list(f: &mut Frame, app: &mut App, area: Rect) {
    let th    = app.theme;
    let title = format!(" Entries ({}) ", app.filtered.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.border_c()))
        .title(Span::styled(title, Style::default().fg(th.fg_c())));

    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .enumerate()
        .map(|(list_pos, &entry_idx)| {
            let e = &app.all_entries[entry_idx];
            let star = if e.favorite {
                Span::styled("★ ", Style::default().fg(th.star_c()))
            } else {
                Span::raw("  ")
            };
            let badge = Span::styled(
                format!("[{}] ", e.category.label()),
                Style::default().fg(cat_color(&th, &e.category)),
            );

            let mut spans = vec![star, badge];
            if !e.tool.is_empty() {
                spans.push(Span::styled(e.tool.clone(), Style::default().fg(th.accent_c())));
                spans.push(Span::styled(" › ", Style::default().fg(th.dim_c())));
            }
            let idx = app.matches.get(list_pos);
            if idx.map_or(true, |v| v.is_empty()) {
                spans.push(Span::styled(e.title.clone(), Style::default().fg(th.fg_c())));
            } else {
                let set: HashSet<usize> = idx.unwrap().iter().copied().collect();
                for (ci, ch) in e.title.chars().enumerate() {
                    let st = if set.contains(&ci) {
                        Style::default().fg(th.accent_c()).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(th.fg_c())
                    };
                    spans.push(Span::styled(ch.to_string(), st));
                }
            }
            if e.danger {
                spans.push(Span::styled("  ⚠", Style::default().fg(th.warn_c())));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    if !app.filtered.is_empty() {
        state.select(Some(app.selected));
    }

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(th.sel_c()).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");

    f.render_stateful_widget(list, area, &mut state);
}

/// Header lines above the entry body in the list's side preview: Title,
/// Category, Tags, Updated, the separator, and one blank line.
const PREVIEW_HEADER_LINES: usize = 6;

fn render_preview(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.border_c()))
        .title(Span::styled(" Preview ", Style::default().fg(th.fg_c())));

    // Only used while a Visual/Visual-Line selection is active in this pane —
    // outside of that the preview always shows the entry from the top, as
    // before.
    let in_visual = matches!(app.mode, Mode::Visual | Mode::VisualLine);
    app.view_height = area.height.saturating_sub(2 + PREVIEW_HEADER_LINES as u16);

    match app.selected_entry() {
        None => {
            f.render_widget(
                Paragraph::new("No entries — press o to add one")
                    .block(block)
                    .style(Style::default().fg(th.dim_c()))
                    .alignment(Alignment::Center),
                area,
            );
        }
        Some(entry) => {
            let cc   = cat_color(&th, &entry.category);
            let sep  = "─".repeat(area.width.saturating_sub(4) as usize);
            let star = if entry.favorite { "  ★" } else { "" };

            let mut lines = vec![
                Line::from(vec![
                    Span::styled("  Title    ", Style::default().fg(th.dim_c())),
                    Span::styled(
                        entry.title.clone(),
                        Style::default().fg(th.fg_c()).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(star.to_string(), Style::default().fg(th.star_c())),
                ]),
                Line::from(vec![
                    Span::styled("  Category ", Style::default().fg(th.dim_c())),
                    Span::styled(entry.category.label(), Style::default().fg(cc)),
                    Span::styled(if entry.tool.is_empty() { "".into() } else { format!("   Tool  {}", entry.tool) }, Style::default().fg(th.accent_c())),
                ]),
                Line::from(vec![
                    Span::styled("  Tags     ", Style::default().fg(th.dim_c())),
                    Span::styled(entry.tags_display(), Style::default().fg(th.accent_c())),
                ]),
                Line::from(vec![
                    Span::styled("  Updated  ", Style::default().fg(th.dim_c())),
                    Span::styled(entry.updated_at.clone(), Style::default().fg(th.dim_c())),
                    if entry.danger {
                        Span::styled("   ⚠ destructive — Y then confirm, don't paste blindly", Style::default().fg(th.warn_c()))
                    } else {
                        Span::raw("")
                    },
                ]),
                Line::from(Span::styled(sep, Style::default().fg(th.dim_c()))),
                Line::from(""),
            ];
            lines.extend(highlight_markdown(&th, &entry.body()));

            if in_visual {
                let (a, b) = ordered(app.visual_anchor_line, app.cursor_line);
                for i in a..=b {
                    let idx = PREVIEW_HEADER_LINES + i;
                    if let Some(line) = lines.get_mut(idx) {
                        *line = tint_line(std::mem::replace(line, Line::default()), th.sel_c());
                    }
                }
            }

            let scroll = if in_visual { app.view_scroll } else { 0 };
            f.render_widget(
                Paragraph::new(lines).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0)),
                area,
            );
        }
    }
}

/// Header lines rendered above the entry body in the full view: the
/// Tags/Updated line, the separator, and one blank line.
const VIEW_HEADER_LINES: usize = 3;

fn ordered(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Tint every span on a line with a selection background, preserving each
/// span's own foreground/modifiers.
fn tint_line(line: Line<'static>, bg: Color) -> Line<'static> {
    let spans = line
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content, s.style.bg(bg)))
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Flip the colors of the single character at `col` to draw a Vim-style
/// block cursor, so the cursor position is visible even outside Visual mode
/// (where `tint_line` is the only other thing marking it).
fn cursor_at(line: Line<'static>, col: usize, th: &Theme) -> Line<'static> {
    let cursor_style = Style::default().fg(th.bg_c()).bg(th.fg_c());
    let mut out: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
    let mut idx = 0usize;
    let mut placed = false;
    for span in line.spans {
        let content = span.content.to_string();
        let len = content.chars().count();
        if !placed && col < idx + len {
            let local = col - idx;
            let mut chars = content.chars();
            let before: String = chars.by_ref().take(local).collect();
            let cursor_ch: String = chars.by_ref().take(1).collect();
            let after: String = chars.collect();
            if !before.is_empty() {
                out.push(Span::styled(before, span.style));
            }
            out.push(Span::styled(cursor_ch, cursor_style));
            if !after.is_empty() {
                out.push(Span::styled(after, span.style));
            }
            placed = true;
        } else {
            out.push(span);
        }
        idx += len;
    }
    if !placed {
        // Cursor sits past the end of the line (empty line, or col == len).
        out.push(Span::styled(" ", cursor_style));
    }
    Line::from(out)
}

fn render_view_screen(f: &mut Frame, app: &mut App, area: Rect) {
    let th = app.theme;
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    // borders (2) + the header lines above the body
    app.view_height = rows[0]
        .height
        .saturating_sub(2 + VIEW_HEADER_LINES as u16);

    if let Some(entry) = app.selected_entry() {
        let cc   = cat_color(&th, &entry.category);
        let star = if entry.favorite { " ★" } else { "" };
        let name = if entry.tool.is_empty() {
            entry.title.clone()
        } else {
            format!("{} › {}", entry.tool, entry.title)
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(cc))
            .title(Span::styled(
                format!("  [{}]  {}{}  ", entry.category.label(), name, star),
                Style::default().fg(cc).add_modifier(Modifier::BOLD),
            ));

        let sep = "─".repeat(area.width.saturating_sub(4) as usize);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("Tags: ", Style::default().fg(th.dim_c())),
                Span::styled(entry.tags_display(), Style::default().fg(th.accent_c())),
                Span::raw("   "),
                Span::styled("Updated: ", Style::default().fg(th.dim_c())),
                Span::styled(entry.updated_at.clone(), Style::default().fg(th.dim_c())),
                if entry.danger {
                    Span::styled("   ⚠ destructive", Style::default().fg(th.warn_c()))
                } else {
                    Span::raw("")
                },
            ]),
            Line::from(Span::styled(sep, Style::default().fg(th.dim_c()))),
            Line::from(""),
        ];
        lines.extend(highlight_markdown(&th, &entry.body()));

        // Visual / Visual-Line highlight the lines the selection touches.
        // (Char-wise Visual tints the whole line; the actual yank still
        // copies only the exact character span — see `yank_visual`.)
        if matches!(app.mode, Mode::Visual | Mode::VisualLine) {
            let (a, b) = ordered(app.visual_anchor_line, app.cursor_line);
            for i in a..=b {
                let idx = VIEW_HEADER_LINES + i;
                if let Some(line) = lines.get_mut(idx) {
                    *line = tint_line(std::mem::replace(line, Line::default()), th.sel_c());
                }
            }
        }

        // A persistent block cursor at the current line/column — visible in
        // Normal mode too, not just while a Visual selection is tinted, so
        // it's always clear which line `yy`/`i` will act on.
        if matches!(app.mode, Mode::Normal | Mode::Visual | Mode::VisualLine) {
            let idx = VIEW_HEADER_LINES + app.cursor_line;
            if let Some(line) = lines.get_mut(idx) {
                *line = cursor_at(std::mem::replace(line, Line::default()), app.cursor_col, &th);
            }
        }

        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false })
                .scroll((app.view_scroll, 0)),
            rows[0],
        );
    }
    render_statusline(f, app, rows[1]);
}

// ─── add / edit form ───────────────────────────────────────────────────────────

fn render_form(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let w = 78u16.min(area.width.saturating_sub(4));
    let h = 24u16.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    f.render_widget(Clear, popup);

    let editing = app.form.editing_id.is_some();
    let dirty   = app.form.is_dirty();
    let title_text = match (editing, dirty) {
        (true,  true)  => "  Edit Entry [+]  ",
        (true,  false) => "  Edit Entry  ",
        (false, true)  => "  New Entry [+]  ",
        (false, false) => "  New Entry  ",
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if dirty { th.warn_c() } else { th.accent_c() }))
        .title(Span::styled(
            title_text,
            Style::default()
                .fg(if dirty { th.warn_c() } else { th.accent_c() })
                .add_modifier(Modifier::BOLD),
        ));
    f.render_widget(outer, popup);

    let inner = Rect::new(
        popup.x + 1,
        popup.y + 1,
        popup.width.saturating_sub(2),
        popup.height.saturating_sub(2),
    );
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(0),
        ])
        .split(inner);

    let form   = &app.form;
    let insert = app.mode == Mode::Insert;
    let cursor = |focused: bool| if focused && insert { "_" } else { "" };

    input_field(f, th, rows[0], "Title", &form.title,
        form.focused == FormField::Title, cursor(form.focused == FormField::Title));

    {
        let on = form.focused == FormField::Category;
        let bs = if on { Style::default().fg(th.accent_c()) } else { Style::default().fg(th.border_c()) };
        let block = Block::default().borders(Borders::ALL).border_style(bs).title(" Category  (h/l) ");
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("  ◀  {}  ▶", form.category.label()),
                Style::default().fg(cat_color(th, &form.category)),
            ))
            .block(block),
            rows[1],
        );
    }

    input_field(f, th, rows[2], "Tags  (comma-separated)", &form.tags,
        form.focused == FormField::Tags, cursor(form.focused == FormField::Tags));

    {
        let on = form.focused == FormField::Content;
        let bs = if on { Style::default().fg(th.accent_c()) } else { Style::default().fg(th.border_c()) };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(bs)
            .title(" Content  [:editor for $EDITOR] ");
        let body = if on && insert { format!("{}_", form.content) } else { form.content.clone() };
        f.render_widget(
            Paragraph::new(body).block(block).wrap(Wrap { trim: false }).style(Style::default().fg(th.fg_c())),
            rows[3],
        );
    }
}

fn input_field(f: &mut Frame, th: &Theme, area: Rect, label: &str, value: &str, focused: bool, cursor: &str) {
    let bs = if focused { Style::default().fg(th.accent_c()) } else { Style::default().fg(th.border_c()) };
    let block = Block::default().borders(Borders::ALL).border_style(bs).title(format!(" {} ", label));
    f.render_widget(
        Paragraph::new(Span::styled(
            format!("{}{}", value, cursor),
            Style::default().fg(th.fg_c()),
        ))
        .block(block),
        area,
    );
}

// ─── shared bottom statusline ────────────────────────────────────────────────

fn render_statusline(f: &mut Frame, app: &App, area: Rect) {
    let th = &app.theme;
    let line = match app.mode {
        Mode::Command => Line::from(Span::styled(
            format!(":{}_", app.cmdline),
            Style::default().fg(th.fg_c()),
        )),
        Mode::Search => Line::from(Span::styled(
            format!("/{}_", app.cmdline),
            Style::default().fg(th.fg_c()),
        )),
        _ => {
            if let Some(ref msg) = app.status {
                Line::from(Span::styled(format!(" {}", msg), Style::default().fg(th.accent_c())))
            } else {
                let mode_badge = Span::styled(
                    format!(" -- {} -- ", app.mode.label()),
                    Style::default().fg(th.dim_c()).add_modifier(Modifier::BOLD),
                );
                let hint = match (app.screen, app.mode) {
                    (Screen::List, Mode::Visual) | (Screen::View, Mode::Visual) =>
                        "j/k/h/l extend · y / +y yank selection · v or Esc to exit",
                    (Screen::List, Mode::VisualLine) | (Screen::View, Mode::VisualLine) =>
                        "j/k extend lines · y / +y yank lines · V or Esc to exit",
                    (Screen::List, _) =>
                        "j/k move · gt/gT tabs · v/V select · / search · : cmd · o new · e edit · dd del · u undo · ? help",
                    (Screen::View, _) =>
                        "j/k scroll · gg/G ends · yy yank line · i edit · v/V visual · f fav · q back · : cmd",
                    (Screen::Form, Mode::Insert) =>
                        "type… · Esc → normal · Enter newline in content",
                    (Screen::Form, _) =>
                        "i insert · Tab/j/k field · h/l category · :w save · :q cancel (:q! discard)",
                };
                Line::from(vec![mode_badge, Span::styled(hint, Style::default().fg(th.dim_c()))])
            }
        }
    };
    f.render_widget(Paragraph::new(line), area);
}

// ─── help overlay ────────────────────────────────────────────────────────────

fn render_help(f: &mut Frame, th: &Theme, area: Rect) {
    let w = 68u16.min(area.width.saturating_sub(4));
    let h = 30u16.min(area.height.saturating_sub(2));
    let x = (area.width.saturating_sub(w)) / 2;
    let y = (area.height.saturating_sub(h)) / 2;
    let popup = Rect::new(x, y, w, h);

    f.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(th.accent_c()))
        .title(Span::styled(
            "  Help — Vim keys & commands  ",
            Style::default().fg(th.accent_c()).add_modifier(Modifier::BOLD),
        ));

    let dim = Style::default().fg(th.dim_c());
    let acc = Style::default().fg(th.accent_c());
    let wht = Style::default().fg(th.fg_c());
    let hd  = |s: &str| {
        Line::from(Span::styled(
            s.to_string(),
            Style::default().fg(th.head_c(2)).add_modifier(Modifier::BOLD),
        ))
    };
    let kv = |k: &str, v: &str| {
        Line::from(vec![
            Span::styled(format!("  {:<12}", k), acc),
            Span::styled(v.to_string(), wht),
        ])
    };

    let lines = vec![
        hd(" NORMAL — list"),
        kv("j / k", "move down / up"),
        kv("gg / G", "top / bottom"),
        kv("gt / gT", "next / previous tab (ALL/CMD/NOTE/TOOL/FAV)"),
        kv("Ctrl-d/u", "half page down / up"),
        kv("l / Enter", "open full view"),
        kv("o / e", "new entry / edit selected"),
        kv("dd", "delete selected"),
        kv("u / Ctrl-r", "undo / redo"),
        kv("yy", "yank whole content to clipboard"),
        kv("Y", "yank just the command, placeholders filled"),
        kv("f", "toggle favorite (undoable)"),
        kv("/  then n/N", "search (FTS prefix) · next / prev match"),
        kv("v / V", "Visual / Visual-Line — select in the preview pane"),
        kv("q", "quit"),
        Line::from(Span::styled("  arrow keys are disabled — use h j k l", dim)),
        Line::from(""),
        hd(" NORMAL — full view"),
        kv("j/k gg/G", "move cursor (shown as a block) · scrolls to follow"),
        kv("yy / +y", "yank the line under the cursor"),
        kv("i", "edit this entry (opens the form, cursor in Content)"),
        kv("v", "Visual mode — char-wise selection"),
        kv("V", "Visual-Line mode — whole-line selection"),
        Line::from(""),
        hd(" VISUAL / V-LINE — list preview & full view"),
        Line::from(Span::styled(
            "  j/k/gg/G extend · h/l move within a line (char-wise)\n  y or +y yanks the exact selection — no extra lines or\n  spaces · v/V/Esc exits without yanking",
            wht,
        )),
        Line::from(""),
        hd(" NORMAL — form"),
        kv("i / a", "enter insert mode"),
        kv("Tab / j/k", "next / prev field"),
        kv("h / l", "cycle category"),
        kv("Esc", "cancel form (blocked if unsaved)"),
        Line::from(""),
        hd(" COMMANDS ( : )"),
        Line::from(Span::styled("  :w  :q  :q!  :wq  :x   :d   :u   :redo   :new   :e", wht)),
        Line::from(Span::styled("  :sort [key] [!]   :cat all|command|note|tool", wht)),
        Line::from(Span::styled("  :fav   :favorites   :noh   :editor   :import <path>", wht)),
        Line::from(Span::styled("  :g/pattern/d   :g/pattern/fav   :g/pattern/unfav", wht)),
        Line::from(Span::styled("  :dedupe [exact|title|normalized] [longest|first|last]  (! applies)", wht)),
        Line::from(Span::styled("  :set <name> <value>   :unset <name>   :vars", wht)),
        Line::from(Span::styled("  :theme [name]   :help", wht)),
        Line::from(""),
        Line::from(Span::styled("  press ? or Esc to close", dim)),
    ];

    f.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: false }), popup);
}
