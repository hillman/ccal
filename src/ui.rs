//! Rendering. Reads `App` state; never touches the store directly.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use crate::app::{App, Entry, Mode, Tab};

pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_tabs(f, app, chunks[0]);
    match &app.mode {
        Mode::NoteEdit { title, .. } => {
            let block = Block::default()
                .borders(Borders::ALL)
                .title(format!(" {title} (markdown) "));
            let mut ta = app.editor.clone();
            ta.set_block(block);
            f.render_widget(&ta, chunks[1]);
        }
        _ => match app.tab {
            Tab::Todos => draw_todos(f, app, chunks[1]),
            Tab::Notes => draw_notes(f, app, chunks[1]),
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
    ];
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .block(Block::default().borders(Borders::ALL).title(" ccal ")),
        area,
    );
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
    let at_root = app.cur.is_empty();
    let mut items: Vec<ListItem> = Vec::new();
    if !at_root {
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
            Entry::Note { title, .. } => ListItem::new(format!("  📄 {title}")),
        });
    }

    let crumb = if at_root {
        "/".to_string()
    } else {
        format!("/{}", app.cur.join("/"))
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " Notes {crumb}  (n new · r reload · Enter open · d del) "
        )))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");
    let mut state = ListState::default();
    let rows = app.entries.len() + if at_root { 0 } else { 1 };
    if rows > 0 {
        state.select(Some(app.entry_sel));
    }
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let line = match &app.mode {
        Mode::Input { buffer, .. } => Line::from(vec![
            Span::styled("› ", Style::default().fg(Color::Yellow)),
            Span::raw(buffer.as_str()),
            Span::styled("▏", Style::default().fg(Color::Yellow)),
        ]),
        _ => Line::from(Span::styled(
            app.status.as_str(),
            Style::default().fg(Color::DarkGray),
        )),
    };
    f.render_widget(Paragraph::new(line), area);
}
