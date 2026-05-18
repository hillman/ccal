//! Rendering. Reads `App` state; never touches the store directly.

use edtui::{EditorMode, EditorTheme, EditorView, SyntaxHighlighter};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use chrono::Local;

use crate::app::{App, Entry, Mode, Preview, Prompt, Tab};

pub fn draw(f: &mut Frame, app: &App) {
    // While taking text input, grow the bottom strip to two rows so a dim
    // help line can sit directly above the field being typed into.
    let inputting = matches!(app.mode, Mode::Input { .. });
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(if inputting { 2 } else { 1 }),
        ])
        .split(f.area());

    // Reset the per-frame clickable-list region; whichever view owns a
    // selectable list re-records it below. Cleared views (editor, calendar
    // agenda) thus correctly have no list hit-target.
    app.clear_list_hit();

    draw_tabs(f, app, chunks[0]);
    match &app.mode {
        Mode::NoteEdit { title, .. } => {
            let mode = match app.editor.mode {
                EditorMode::Normal => "NORMAL",
                EditorMode::Insert => "INSERT",
                EditorMode::Visual => "VISUAL",
                EditorMode::Search => "SEARCH",
            };
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} — {mode}  (md) "));
            let theme = EditorTheme::default().block(block).hide_status_line();
            // EditorView needs &mut state; clone for rendering (state of
            // record lives in App and is mutated via the event handler).
            let mut ed = app.editor.clone();
            let view = EditorView::new(&mut ed)
                .theme(theme)
                .syntax_highlighter(SyntaxHighlighter::new("dracula", "md").ok())
                .wrap(true);
            f.render_widget(view, chunks[1]);
        }
        _ => match app.tab {
            Tab::Dashboard => draw_dashboard(f, app, chunks[1]),
            Tab::Todos => draw_todos(f, app, chunks[1]),
            Tab::Notes => draw_notes(f, app, chunks[1]),
            Tab::Calendar => draw_calendar(f, app, chunks[1]),
            Tab::History => draw_history(f, app, chunks[1]),
        },
    }
    draw_status(f, app, chunks[2]);
}

/// Border-stripped inner area of a `Borders::ALL` block (where its content
/// actually renders) — used to map a click back to a row/label.
fn inner(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let sel = Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
    let unsel = Style::default().fg(Color::Cyan);
    let labels = [
        (" [1] Dashboard ", Tab::Dashboard),
        (" [2] Todos ", Tab::Todos),
        (" [3] Notes ", Tab::Notes),
        (" [4] Calendar ", Tab::Calendar),
        (" [5] History ", Tab::History),
    ];
    // Build the spans and, in lockstep, each label's inclusive x-range so
    // a click on the tab bar maps back to its tab. Content starts one cell
    // in from the border; labels are separated by a 2-space raw gap.
    let mut spans: Vec<Span> = Vec::new();
    let mut hits: Vec<(u16, u16, Tab)> = Vec::new();
    let mut x = area.x + 1;
    for (i, (label, tab)) in labels.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
            x += 2;
        }
        let w = label.chars().count() as u16;
        hits.push((x, x + w - 1, *tab));
        x += w;
        spans.push(Span::styled(
            *label,
            if app.tab == *tab { sel } else { unsel },
        ));
    }
    app.record_tabs(area.y + 1, hits);
    let mut block = Block::default().borders(Borders::ALL).title(" ccal ");
    if let Some(s) = app.sync_indicator() {
        let online = s.starts_with('●');
        let style = Style::default().fg(if online { Color::Green } else { Color::DarkGray });
        block = block.title_top(Line::from(Span::styled(format!(" {s} "), style)).right_aligned());
    }
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

/// Position-0 overview. Left half: the five most-recently-edited notes
/// (the only interactive pane — ↑↓ select, Enter opens it in the Notes
/// editor). Right half: top todos over today's agenda, both read-only and
/// reusing the same `app.todos` / `app.cal_occ` the dedicated tabs render.
fn draw_dashboard(f: &mut Frame, app: &App, area: Rect) {
    let halves = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Left: recent notes (full height, selectable).
    let note_items: Vec<ListItem> = if app.dash_notes.is_empty() {
        vec![ListItem::new(Span::styled(
            "  No notes yet",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.dash_notes
            .iter()
            .map(|m| {
                let title = if m.title.is_empty() { "(untitled)" } else { &m.title };
                let where_ = if m.folder.is_empty() {
                    "/".to_string()
                } else {
                    format!("/{}", m.folder.join("/"))
                };
                let icon = if m.private { "🔒" } else { "📄" };
                let line = Line::from(vec![
                    Span::raw(format!("  {icon} ")),
                    Span::styled(
                        title.to_string(),
                        if m.private {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        format!("  ⟨{where_}⟩"),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]);
                ListItem::new(line)
            })
            .collect()
    };
    let notes = List::new(note_items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Recent notes  (↑↓ select · Enter open) "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut nstate = ListState::default();
    if !app.dash_notes.is_empty() {
        nstate.select(Some(app.dash_sel));
    }
    f.render_stateful_widget(notes, halves[0], &mut nstate);
    app.record_list(inner(halves[0]), nstate.offset(), app.dash_notes.len());

    // Right: todos over today's agenda, half height each.
    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(halves[1]);

    let todo_items: Vec<ListItem> = if app.todos.is_empty() {
        vec![ListItem::new(Span::styled(
            "  Nothing to do",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.todos
            .iter()
            .take(5)
            .map(|t| ListItem::new(format!("• {}", t.text)))
            .collect()
    };
    f.render_widget(
        List::new(todo_items)
            .block(Block::default().borders(Borders::ALL).title(" Top todos ")),
        right[0],
    );

    let today = Local::now().date_naive();
    let line = |o: &ccal::calendar::Occurrence| -> String {
        let s = o.start.with_timezone(&Local);
        if o.all_day {
            format!("  • all day   {}", o.summary)
        } else {
            let e = o.end.with_timezone(&Local);
            format!("  {}–{}  {}", s.format("%H:%M"), e.format("%H:%M"), o.summary)
        }
    };
    let mut cal_items: Vec<ListItem> = Vec::new();
    for (label, day) in [
        ("Today", today),
        ("Tomorrow", today + chrono::Duration::days(1)),
    ] {
        cal_items.push(ListItem::new(Span::styled(
            format!("{label} — {}", day.format("%a %-d %b")),
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
        let mut hits = 0;
        for o in &app.cal_occ {
            if o.start.with_timezone(&Local).date_naive() == day {
                cal_items.push(ListItem::new(line(o)));
                hits += 1;
            }
        }
        if hits == 0 {
            cal_items.push(ListItem::new(Span::styled(
                "  (nothing scheduled)",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    f.render_widget(
        List::new(cal_items)
            .block(Block::default().borders(Borders::ALL).title(" Agenda ")),
        right[1],
    );
}

fn draw_todos(f: &mut Frame, app: &App, area: Rect) {
    // Tags are rendered as a right-aligned column rather than trailing the
    // text, so the list reads cleanly down the left edge. Usable width =
    // area minus the block borders (2) and the highlight-symbol gutter (2,
    // reserved on every row).
    let width = area.width.saturating_sub(4) as usize;
    let items: Vec<ListItem> = app
        .todos
        .iter()
        .map(|t| {
            let marked = app.todo_marks.contains(&t.id);
            let marker = if marked { "◉ " } else { "• " };
            let tags = if t.tags.is_empty() {
                String::new()
            } else {
                t.tags.iter().map(|tag| format!("#{tag}")).collect::<Vec<_>>().join(" ")
            };
            let tags_w = tags.chars().count();
            // Reserve the tag column (plus a two-space gutter) on the right;
            // truncate the text with an ellipsis if the row is too narrow.
            let reserved = if tags_w == 0 { 0 } else { tags_w + 2 };
            let text_room = width.saturating_sub(marker.chars().count() + reserved);
            let mut text = t.text.clone();
            if text.chars().count() > text_room {
                text = text
                    .chars()
                    .take(text_room.saturating_sub(1))
                    .collect::<String>()
                    + "…";
            }
            let used = marker.chars().count() + text.chars().count() + reserved;
            let pad = " ".repeat(width.saturating_sub(used));
            let spans = vec![
                Span::styled(
                    marker,
                    if marked {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ),
                Span::raw(text),
                Span::raw(pad),
                Span::styled(tags, Style::default().fg(Color::Magenta)),
            ];
            ListItem::new(Line::from(spans))
        })
        .collect();
    let title = match &app.tag_filter {
        Some(tag) => format!(
            " Todos  ▶ #{tag} ◀  ({} shown · Esc clears · t tag · f filter · Space sel) ",
            app.todos.len()
        ),
        None => {
            " Todos  (a add · e edit · d del · J/K move · Space select · t tag · f filter) "
                .to_string()
        }
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.todos.is_empty() {
        state.select(Some(app.todo_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
    app.record_list(inner(area), state.offset(), app.todos.len());
}

/// One row for a folder/note entry. `dim` greys it out — used in the
/// non-interactive folder preview so it reads as context, not a selection.
fn entry_item(e: &Entry, dim: bool) -> ListItem<'static> {
    match e {
        Entry::Dir(name) => {
            let mut s = Style::default().fg(if dim { Color::DarkGray } else { Color::Blue });
            if !dim {
                s = s.add_modifier(Modifier::BOLD);
            }
            ListItem::new(Span::styled(format!("📁 {name}/"), s))
        }
        Entry::Note { title, private, .. } => {
            if *private {
                ListItem::new(Span::styled(
                    format!("  🔒 {title}"),
                    Style::default().fg(if dim { Color::DarkGray } else { Color::Yellow }),
                ))
            } else if dim {
                ListItem::new(Span::styled(
                    format!("  📄 {title}"),
                    Style::default().fg(Color::DarkGray),
                ))
            } else {
                ListItem::new(format!("  📄 {title}"))
            }
        }
    }
}

fn draw_notes(f: &mut Frame, app: &App, area: Rect) {
    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    // Search collapses folders to a flat cross-corpus hit list, but the
    // selected hit still gets a preview pane like normal navigation.
    if let Mode::Search { query } = &app.mode {
        let items: Vec<ListItem> = app.entries.iter().map(|e| entry_item(e, false)).collect();
        let title =
            format!(" Search “{}”  ({} hits · Esc cancel) ", query, app.entries.len());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("> ");
        let mut state = ListState::default();
        if !app.entries.is_empty() {
            state.select(Some(app.entry_sel));
        }
        f.render_stateful_widget(list, panes[0], &mut state);
        draw_preview(f, app, panes[1]);
        return;
    }

    // --- Left: the navigable list of the current folder ---------------
    let flat = app.flat_list();
    let mut items: Vec<ListItem> = Vec::new();
    if !flat {
        items.push(ListItem::new(Span::styled(
            "📁 ..",
            Style::default().fg(Color::Blue),
        )));
    }
    items.extend(app.entries.iter().map(|e| entry_item(e, false)));

    let crumb = if flat { "/".to_string() } else { format!("/{}", app.cur.join("/")) };
    let title =
        format!(" Notes {crumb}  (n new · R rename · m move · p priv · d del · / search · r reload) ");
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    let rows = app.entries.len() + if flat { 0 } else { 1 };
    if rows > 0 {
        state.select(Some(app.entry_sel));
    }
    f.render_stateful_widget(list, panes[0], &mut state);
    app.record_list(inner(panes[0]), state.offset(), rows);

    // --- Right: context preview of whatever is selected --------------
    draw_preview(f, app, panes[1]);
}

/// Render the right-hand context preview for the current selection.
/// Shared by normal folder navigation and the search hit list.
fn draw_preview(f: &mut Frame, app: &App, area: Rect) {
    match &app.preview {
        Preview::Folder { path, entries } => {
            let title = format!(" /{} ", path.join("/"));
            if entries.is_empty() {
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        "  (empty)",
                        Style::default().fg(Color::DarkGray),
                    )))
                    .block(Block::default().borders(Borders::ALL).title(title)),
                    area,
                );
            } else {
                let items: Vec<ListItem> =
                    entries.iter().map(|e| entry_item(e, true)).collect();
                f.render_widget(
                    List::new(items)
                        .block(Block::default().borders(Borders::ALL).title(title)),
                    area,
                );
            }
        }
        Preview::Note { title, body, private } => {
            let lock = if *private { "🔒 " } else { "" };
            let text = if body.trim().is_empty() {
                Paragraph::new(Line::from(Span::styled(
                    "  (empty note)",
                    Style::default().fg(Color::DarkGray),
                )))
            } else {
                Paragraph::new(body.as_str())
            };
            f.render_widget(
                text.wrap(ratatui::widgets::Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(" {lock}{title} ")),
                ),
                area,
            );
        }
        Preview::None => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  ↑↓ to browse · → / Enter to open",
                    Style::default().fg(Color::DarkGray),
                )))
                .block(Block::default().borders(Borders::ALL).title(" Preview ")),
                area,
            );
        }
    }
}

/// Compact "time ago" for the history timeline. `ms == 0` → unknown (a
/// pre-timestamp change or genesis).
fn ago(ms: i64) -> String {
    if ms <= 0 {
        return "—".to_string();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let s = (now - ms) / 1000;
    if s < 60 {
        "just now".to_string()
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86400)
    }
}

fn draw_history(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .history
        .iter()
        .map(|h| {
            let when = ago(h.ts);
            match &h.checkpoint {
                Some(reason) => ListItem::new(Span::styled(
                    format!("★ {reason}   ({when} · {} ops)", h.ops),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                )),
                None => ListItem::new(Span::styled(
                    format!("· {} ops · {when} · {}", h.ops, h.actor),
                    Style::default().fg(Color::DarkGray),
                )),
            }
        })
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(
            " History  (↑↓ select · p preview · r restore whole corpus · c name snapshot) ",
        ))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.history.is_empty() {
        state.select(Some(app.hist_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
    app.record_list(inner(area), state.offset(), app.history.len());
}

/// The Calendar tab: a read-only agenda (Today, then the next 7 days), or
/// the manage sub-view (subscriptions + fetch status). All event times are
/// rendered in the local timezone.
fn draw_calendar(f: &mut Frame, app: &App, area: Rect) {
    if app.cal_manage {
        draw_cal_manage(f, app, area);
        return;
    }

    let today = Local::now().date_naive();
    let day_label = |d: chrono::NaiveDate| d.format("%a %-d %b").to_string();

    // Bucket occurrences by their *local* start date (already sorted by
    // absolute start, which stays ordered after the offset).
    let line = |o: &ccal::calendar::Occurrence| -> String {
        let s = o.start.with_timezone(&Local);
        if o.all_day {
            format!("  • all day   {}", o.summary)
        } else {
            let e = o.end.with_timezone(&Local);
            let loc = if o.location.is_empty() {
                String::new()
            } else {
                format!("   · {}", o.location)
            };
            format!("  {}–{}  {}{}", s.format("%H:%M"), e.format("%H:%M"), o.summary, loc)
        }
    };

    let mut items: Vec<ListItem> = Vec::new();
    let section = |items: &mut Vec<ListItem>, title: String| {
        items.push(ListItem::new(Span::styled(
            title,
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        )));
    };

    section(&mut items, format!("Today — {}", day_label(today)));
    let mut today_hits = 0;
    for o in &app.cal_occ {
        if o.start.with_timezone(&Local).date_naive() == today {
            items.push(ListItem::new(line(o)));
            today_hits += 1;
        }
    }
    if today_hits == 0 {
        items.push(ListItem::new(Span::styled(
            "  (nothing scheduled)",
            Style::default().fg(Color::DarkGray),
        )));
    }

    items.push(ListItem::new(""));
    section(&mut items, "This week".to_string());
    for n in 1..=7 {
        let d = today + chrono::Duration::days(n);
        section(&mut items, format!("  {}", day_label(d)));
        let mut hits = 0;
        for o in &app.cal_occ {
            if o.start.with_timezone(&Local).date_naive() == d {
                items.push(ListItem::new(line(o)));
                hits += 1;
            }
        }
        if hits == 0 {
            items.push(ListItem::new(Span::styled(
                "    —",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    let n = app.cal_subs.len();
    let title = format!(
        " Calendar — {n} subscribed  (a add · r refresh · m manage) "
    );
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn draw_cal_manage(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = if app.cal_subs.is_empty() {
        vec![ListItem::new(Span::styled(
            "  No calendars — press a to paste an ICS URL",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.cal_subs
            .iter()
            .map(|c| {
                let st = app.cal_status.iter().find(|s| s.id == c.id);
                let name = if c.name.is_empty() { c.url.clone() } else { c.name.clone() };
                let (detail, style) = match st {
                    Some(s) if s.error.is_some() => (
                        format!("⚠ {}", s.error.as_deref().unwrap_or("error")),
                        Style::default().fg(Color::Red),
                    ),
                    Some(s) if s.last_ok > 0 => (
                        format!("{} events · synced {}", s.events, ago(s.last_ok)),
                        Style::default().fg(Color::Green),
                    ),
                    _ => ("fetching…".to_string(), Style::default().fg(Color::DarkGray)),
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("📅 {name}  ")),
                    Span::styled(detail, style),
                ]))
            })
            .collect()
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(
            " Manage calendars  (↑↓ select · d delete · a add · m back to agenda) ",
        ))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.cal_subs.is_empty() {
        state.select(Some(app.cal_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
    app.record_list(inner(area), state.offset(), app.cal_subs.len());
}

/// One-line hint shown above the input field, tailored to what's being
/// asked. Kept here (presentation) rather than in `app` so the state
/// machine stays free of UI copy.
fn input_hint(prompt: &Prompt) -> &'static str {
    match prompt {
        Prompt::AddTodo => "New todo  ·  Enter save  ·  Esc cancel",
        Prompt::EditTodo(_) => "Edit todo  ·  Enter save  ·  Esc cancel",
        Prompt::TagTodos => {
            "Tag the selected/marked todos  ·  Tab autocompletes  ·  \
             Enter  ·  Esc"
        }
        Prompt::FilterTag => {
            "Filter todos by tag  ·  Tab autocompletes  ·  empty Enter \
             clears  ·  Esc"
        }
        Prompt::NewNote => {
            "Note name — type folder/note (or folder\\note) to file it; \
             the folders are created  ·  Enter  ·  Esc"
        }
        Prompt::RenameNote(_) => "New note title  ·  Enter save  ·  Esc cancel",
        Prompt::MoveNote(_) => {
            "Move to folder path — a/b  ·  blank = root; missing folders \
             are created  ·  Enter  ·  Esc"
        }
        Prompt::RenameFolder(_) => {
            "New folder name — one component, renames the whole subtree  ·  \
             Enter  ·  Esc"
        }
        Prompt::NewCheckpoint => {
            "Snapshot reason — what is this restore point?  ·  Enter  ·  Esc"
        }
        Prompt::AddCalendar => {
            "Paste an ICS URL (Google “secret iCal” / Proton published \
             link)  ·  Enter subscribe  ·  Esc"
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    match &app.mode {
        Mode::Input { buffer, prompt } => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Length(1)])
                .split(area);
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    input_hint(prompt),
                    Style::default().fg(Color::DarkGray),
                ))),
                rows[0],
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("› ", Style::default().fg(Color::Yellow)),
                    Span::raw(buffer.as_str()),
                    Span::styled("▏", Style::default().fg(Color::Yellow)),
                ])),
                rows[1],
            );
        }
        _ => f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                app.status.as_str(),
                Style::default().fg(Color::DarkGray),
            ))),
            area,
        ),
    }
}
