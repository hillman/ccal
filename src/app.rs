//! TUI state and key handling. Talks only to `ccal::store` / `ccal::models`.
//! The notes "folders" are a virtual tree derived from each note's
//! `folder` array — there is no folder entity and no filesystem.

use anyhow::Result;
use edtui::{EditorEventHandler, EditorMode, EditorState, Lines};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use ccal::models::{NoteMeta, Todo};
use ccal::store::Store;

#[derive(PartialEq, Clone, Copy)]
pub enum Tab {
    Todos,
    Notes,
}

pub enum Prompt {
    AddTodo,
    EditTodo(String),
    NewNote,
}

pub enum Mode {
    Normal,
    Input { prompt: Prompt, buffer: String },
    NoteEdit { id: String, title: String },
}

#[derive(Clone)]
pub enum Entry {
    Dir(String),
    Note { id: String, title: String },
}

pub struct App {
    store: Store,
    pub tab: Tab,
    pub mode: Mode,
    pub should_quit: bool,
    pub status: String,

    pub todos: Vec<Todo>,
    pub todo_sel: usize,

    /// Current notes folder path ([] = root).
    pub cur: Vec<String>,
    pub entries: Vec<Entry>,
    pub entry_sel: usize,
    pub editor: EditorState,
    /// Persisted across keystrokes — holds multi-key Vim state (e.g. `dd`).
    edit_events: EditorEventHandler,
}

impl App {
    pub fn new() -> Result<Self> {
        let store = Store::open()?;
        let mut app = Self {
            store,
            tab: Tab::Todos,
            mode: Mode::Normal,
            should_quit: false,
            status: "Tab: switch · q: quit".into(),
            todos: Vec::new(),
            todo_sel: 0,
            cur: Vec::new(),
            entries: Vec::new(),
            entry_sel: 0,
            editor: EditorState::new(Lines::default()),
            edit_events: EditorEventHandler::default(),
        };
        app.refresh();
        Ok(app)
    }

    fn persist(&mut self) {
        if let Err(e) = self.store.save() {
            self.status = format!("Save failed: {e}");
        }
    }

    fn refresh(&mut self) {
        self.todos = self.store.todos();
        self.entries = self.build_entries();
        self.clamp();
    }

    /// Reload the document from disk (picks up an external `import-bear`).
    fn reload(&mut self) {
        match Store::open() {
            Ok(s) => {
                self.store = s;
                self.refresh();
                self.status = "Reloaded from disk".into();
            }
            Err(e) => self.status = format!("Reload failed: {e}"),
        }
    }

    fn build_entries(&self) -> Vec<Entry> {
        let notes = self.store.note_metas();
        let depth = self.cur.len();
        let mut dirs: Vec<String> = Vec::new();
        let mut here: Vec<&NoteMeta> = Vec::new();
        for n in &notes {
            if n.folder.len() == depth && n.folder == self.cur {
                here.push(n);
            } else if n.folder.len() > depth && n.folder[..depth] == self.cur[..] {
                let name = n.folder[depth].clone();
                if !dirs.contains(&name) {
                    dirs.push(name);
                }
            }
        }
        dirs.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        here.sort_by(|a, b| b.modified.cmp(&a.modified).then_with(|| a.title.cmp(&b.title)));

        let mut out: Vec<Entry> = dirs.into_iter().map(Entry::Dir).collect();
        out.extend(here.into_iter().map(|n| Entry::Note {
            id: n.id.clone(),
            title: if n.title.is_empty() { "(untitled)".into() } else { n.title.clone() },
        }));
        out
    }

    fn at_root(&self) -> bool {
        self.cur.is_empty()
    }
    fn note_rows(&self) -> usize {
        self.entries.len() + if self.at_root() { 0 } else { 1 }
    }

    fn clamp(&mut self) {
        if self.todo_sel >= self.todos.len() {
            self.todo_sel = self.todos.len().saturating_sub(1);
        }
        let rows = self.note_rows();
        if self.entry_sel >= rows {
            self.entry_sel = rows.saturating_sub(1);
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Result<()> {
        match &self.mode {
            Mode::Normal => self.normal_key(key)?,
            Mode::Input { .. } => self.input_key(key)?,
            Mode::NoteEdit { .. } => self.editor_key(key)?,
        }
        self.clamp();
        Ok(())
    }

    // ---- Normal --------------------------------------------------------

    fn normal_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.tab = match self.tab {
                    Tab::Todos => Tab::Notes,
                    Tab::Notes => Tab::Todos,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            _ => match self.tab {
                Tab::Todos => self.todos_key(key)?,
                Tab::Notes => self.notes_key(key)?,
            },
        }
        Ok(())
    }

    fn move_sel(&mut self, delta: isize) {
        let rows = self.note_rows();
        let (sel, len) = match self.tab {
            Tab::Todos => (&mut self.todo_sel, self.todos.len()),
            Tab::Notes => (&mut self.entry_sel, rows),
        };
        if len == 0 {
            return;
        }
        *sel = ((*sel as isize + delta).rem_euclid(len as isize)) as usize;
    }

    fn todos_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('a') => {
                self.mode = Mode::Input { prompt: Prompt::AddTodo, buffer: String::new() };
                self.status = "New todo — Enter: save · Esc: cancel".into();
            }
            KeyCode::Char('e') | KeyCode::Enter if !self.todos.is_empty() => {
                let t = &self.todos[self.todo_sel];
                self.mode = Mode::Input {
                    prompt: Prompt::EditTodo(t.id.clone()),
                    buffer: t.text.clone(),
                };
                self.status = "Edit todo — Enter: save · Esc: cancel".into();
            }
            KeyCode::Char('d') if !self.todos.is_empty() => {
                let id = self.todos[self.todo_sel].id.clone();
                self.store.delete_todo(&id)?;
                self.persist();
                self.refresh();
                self.status = "Todo deleted".into();
            }
            KeyCode::Char('J') if !self.todos.is_empty() => {
                let i = self.todo_sel;
                if i + 1 < self.todos.len() {
                    let (a, b) = (self.todos[i].id.clone(), self.todos[i + 1].id.clone());
                    self.store.swap_todo_order(&a, &b)?;
                    self.persist();
                    self.refresh();
                    self.todo_sel = i + 1;
                }
            }
            KeyCode::Char('K') if !self.todos.is_empty() => {
                let i = self.todo_sel;
                if i > 0 {
                    let (a, b) = (self.todos[i].id.clone(), self.todos[i - 1].id.clone());
                    self.store.swap_todo_order(&a, &b)?;
                    self.persist();
                    self.refresh();
                    self.todo_sel = i - 1;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// `None` = the ".." row; otherwise the selected entry.
    fn selected(&self) -> Option<&Entry> {
        if self.at_root() {
            self.entries.get(self.entry_sel)
        } else if self.entry_sel == 0 {
            None
        } else {
            self.entries.get(self.entry_sel - 1)
        }
    }

    fn notes_key(&mut self, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('r') => self.reload(),
            KeyCode::Char('n') => {
                self.mode = Mode::Input { prompt: Prompt::NewNote, buffer: String::new() };
                self.status = "Note name — Enter: create · Esc: cancel".into();
            }
            KeyCode::Enter | KeyCode::Right => match self.selected().cloned() {
                None => {
                    self.cur.pop();
                    self.entry_sel = 0;
                    self.entries = self.build_entries();
                }
                Some(Entry::Dir(name)) => {
                    self.cur.push(name);
                    self.entry_sel = 0;
                    self.entries = self.build_entries();
                }
                Some(Entry::Note { id, title }) => {
                    let body = self.store.note(&id).map(|n| n.body).unwrap_or_default();
                    self.editor = make_editor(&body);
                    self.mode = Mode::NoteEdit { id, title };
                    self.status = "Editing (NORMAL) — i insert · q save & close".into();
                }
            },
            KeyCode::Left | KeyCode::Char('h') if !self.at_root() => {
                self.cur.pop();
                self.entry_sel = 0;
                self.entries = self.build_entries();
            }
            KeyCode::Char('d') => {
                if let Some(Entry::Note { id, title }) = self.selected().cloned() {
                    self.store.delete_note(&id)?;
                    self.persist();
                    self.entries = self.build_entries();
                    self.clamp();
                    self.status = format!("Deleted note '{title}'");
                } else if matches!(self.selected(), Some(Entry::Dir(_))) {
                    self.status = "Folders are derived — delete the notes inside".into();
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- Input ---------------------------------------------------------

    fn input_key(&mut self, key: KeyEvent) -> Result<()> {
        let Mode::Input { prompt, buffer } = &mut self.mode else { return Ok(()) };
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.status = "Cancelled".into();
            }
            KeyCode::Char(c) => buffer.push(c),
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Enter => {
                let text = buffer.trim().to_string();
                match prompt {
                    Prompt::AddTodo => {
                        if !text.is_empty() {
                            self.store.add_todo(&text)?;
                            self.persist();
                            self.refresh();
                            self.todo_sel = self.todos.len().saturating_sub(1);
                        }
                        self.mode = Mode::Normal;
                        self.status = "Todo added".into();
                    }
                    Prompt::EditTodo(id) => {
                        let id = id.clone();
                        if !text.is_empty() {
                            self.store.set_todo_text(&id, &text)?;
                            self.persist();
                            self.refresh();
                        }
                        self.mode = Mode::Normal;
                        self.status = "Todo updated".into();
                    }
                    Prompt::NewNote => {
                        if text.is_empty() {
                            self.mode = Mode::Normal;
                            self.status = "Empty name — cancelled".into();
                        } else {
                            let cur = self.cur.clone();
                            let id = self.store.create_note(&cur, &text)?;
                            self.persist();
                            self.entries = self.build_entries();
                            self.editor = make_editor("");
                            // New note: drop straight into Insert so you
                            // can type immediately.
                            self.editor.mode = EditorMode::Insert;
                            self.mode = Mode::NoteEdit { id, title: text };
                            self.status = "Editing (INSERT) — Esc then q to save & close".into();
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    // ---- Note editor ---------------------------------------------------

    fn save_body(&mut self) -> Result<()> {
        if let Mode::NoteEdit { id, .. } = &self.mode {
            let id = id.clone();
            let content = self.editor.lines.to_string();
            self.store.set_note_body(&id, &content)?;
            self.persist();
        }
        Ok(())
    }

    /// Modal routing: app commands only fire in the editor's Normal mode;
    /// Insert/Visual/Search keystrokes go straight to edtui. This is why
    /// there is no key conflict — typing never triggers app actions.
    fn editor_key(&mut self, key: KeyEvent) -> Result<()> {
        // Ctrl+S saves without leaving the editor, in any mode.
        if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.save_body()?;
            self.status = "Saved".into();
            return Ok(());
        }
        // In Normal mode, Esc / q save and return to the note list. In
        // Insert mode Esc belongs to edtui (Insert -> Normal), so it is
        // deliberately not intercepted here.
        if self.editor.mode == EditorMode::Normal
            && matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
        {
            self.save_body()?;
            self.mode = Mode::Normal;
            self.entries = self.build_entries();
            self.clamp();
            self.status = "Saved · back to list".into();
            return Ok(());
        }
        self.edit_events.on_key_event(key, &mut self.editor);
        Ok(())
    }
}

fn make_editor(content: &str) -> EditorState {
    EditorState::new(Lines::from(content))
}
