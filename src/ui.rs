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
            Tab::Todos => draw_todos(f, app, chunks[1]),
            Tab::Notes => draw_notes(f, app, chunks[1]),
            Tab::Calendar => draw_calendar(f, app, chunks[1]),
            Tab::History => draw_history(f, app, chunks[1]),
        },
    }
    draw_status(f, app, chunks[2]);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let sel = Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
    let unsel = Style::default().fg(Color::Cyan);
    let spans = vec![
        Span::styled(" [1] Todos ", if app.tab == Tab::Todos { sel } else { unsel }),
        Span::raw("  "),
        Span::styled(" [2] Notes ", if app.tab == Tab::Notes { sel } else { unsel }),
        Span::raw("  "),
        Span::styled(" [3] Calendar ", if app.tab == Tab::Calendar { sel } else { unsel }),
        Span::raw("  "),
        Span::styled(" [4] History ", if app.tab == Tab::History { sel } else { unsel }),
    ];
    let mut block = Block::default().borders(Borders::ALL).title(" ccal ");
    if let Some(s) = app.sync_indicator() {
        let online = s.starts_with('●');
        let style = Style::default().fg(if online { Color::Green } else { Color::DarkGray });
        block = block.title_top(Line::from(Span::styled(format!(" {s} "), style)).right_aligned());
    }
    f.render_widget(Paragraph::new(Line::from(spans)).block(block), area);
}

fn draw_todos(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app
        .todos
        .iter()
        .map(|t| ListItem::new(format!("• {}", t.text)))
        .collect();
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Todos  (a add · e edit · d del · J/K move) "),
        )
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    if !app.todos.is_empty() {
        state.select(Some(app.todo_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
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
    // Search collapses folders to a flat cross-corpus hit list — a
    // navigation preview makes no sense there, so keep it single-pane.
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
        f.render_stateful_widget(list, area, &mut state);
        return;
    }

    let panes = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

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

    // --- Right: context preview of whatever is selected --------------
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
                    panes[1],
                );
            } else {
                let items: Vec<ListItem> =
                    entries.iter().map(|e| entry_item(e, true)).collect();
                f.render_widget(
                    List::new(items)
                        .block(Block::default().borders(Borders::ALL).title(title)),
                    panes[1],
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
                panes[1],
            );
        }
        Preview::None => {
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "  ↑↓ to browse · → / Enter to open",
                    Style::default().fg(Color::DarkGray),
                )))
                .block(Block::default().borders(Borders::ALL).title(" Preview ")),
                panes[1],
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
}

/// One-line hint shown above the input field, tailored to what's being
/// asked. Kept here (presentation) rather than in `app` so the state
/// machine stays free of UI copy.
fn input_hint(prompt: &Prompt) -> &'static str {
    match prompt {
        Prompt::AddTodo => "New todo  ·  Enter save  ·  Esc cancel",
        Prompt::EditTodo(_) => "Edit todo  ·  Enter save  ·  Esc cancel",
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
