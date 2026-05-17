//! The `ccal` TUI binary. Bear import lives in a separate binary
//! (`import-bear`) and is intentionally not reachable from here.

mod app;
mod cal_sync;
mod sync_client;
mod ui;

use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};

use app::App;

fn main() -> Result<()> {
    let mut terminal = setup_terminal()?;
    // Only the main UI thread owns the terminal, so only its panic should
    // tear the alternate screen down. A background worker ("ccal-cal" /
    // "ccal-sync") that panics is contained where it runs (see
    // `cal_sync::refresh`'s `catch_unwind`); letting its panic restore the
    // terminal here would corrupt a perfectly live UI — which is what made
    // a bad calendar feed leave the screen wedged.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let worker = std::thread::current()
            .name()
            .is_some_and(|n| n == "ccal-cal" || n == "ccal-sync");
        if worker {
            return;
        }
        let _ = restore_terminal();
        original_hook(info);
    }));

    let res = run(&mut terminal);
    restore_terminal()?;
    res
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    let mut app = App::new()?;
    while !app.should_quit {
        app.tick(); // fold in anything the background sync thread merged
        terminal.draw(|f| ui::draw(f, &app))?;
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == event::KeyEventKind::Press {
                    app.on_key(key)?;
                }
            }
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}
