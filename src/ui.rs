//! Rendering. Reads `App` state; never touches the store directly.

use edtui::{EditorMode, EditorTheme, EditorView, SyntaxHighlighter};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::{App, Entry, Mode, Prompt, Tab};

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
            Tab::History => draw_history(f, app, chunks[1]),
        },
    }
    draw_status(f, app, chunks[2]);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let sel = Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD);
    let unsel = Style::default().fg(Color::Cyan);
    let spans = vec![
        Span::styled(" Todos ", if app.tab == Tab::Todos { sel } else { unsel }),
        Span::raw("  "),
        Span::styled(" Notes ", if app.tab == Tab::Notes { sel } else { unsel }),
        Span::raw("  "),
        Span::styled(" History ", if app.tab == Tab::History { sel } else { unsel }),
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

fn draw_notes(f: &mut Frame, app: &App, area: Rect) {
    let flat = app.flat_list();
    let mut items: Vec<ListItem> = Vec::new();
    if !flat {
        items.push(ListItem::new(Span::styled(
            "📁 ..",
            Style::default().fg(Color::Blue),
        )));
    }
    for e in &app.entries {
        items.push(match e {
            Entry::Dir(name) => ListItem::new(Span::styled(
                format!("📁 {name}/"),
                Style::default().fg(Color::Blue).add_modifier(Modifier::BOLD),
            )),
            Entry::Note { title, private, .. } => {
                if *private {
                    ListItem::new(Span::styled(
                        format!("  🔒 {title}"),
                        Style::default().fg(Color::Yellow),
                    ))
                } else {
                    ListItem::new(format!("  📄 {title}"))
                }
            }
        });
    }

    let title = if let Mode::Search { query } = &app.mode {
        format!(" Search “{}”  ({} hits · Esc cancel) ", query, app.entries.len())
    } else {
        let crumb = if flat {
            "/".to_string()
        } else {
            format!("/{}", app.cur.join("/"))
        };
        format!(" Notes {crumb}  (n new · R rename · m move · p priv · d del · / search · r reload) ")
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    let rows = app.entries.len() + if flat { 0 } else { 1 };
    if rows > 0 {
        state.select(Some(app.entry_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
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
