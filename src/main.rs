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
    event::{self, DisableMouseCapture, EnableMouseCapture, Event},
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
    // Mirrors `app.mouse_on`; the run-loop owns flipping terminal capture
    // when the user toggles it with `M` (capture off restores native
    // drag-to-select/copy).
    let mut mouse_applied = true;
    while !app.should_quit {
        app.tick(); // fold in anything the background sync thread merged
        terminal.draw(|f| ui::draw(f, &app))?;
        if app.mouse_on != mouse_applied {
            if app.mouse_on {
                execute!(io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(io::stdout(), DisableMouseCapture)?;
            }
            mouse_applied = app.mouse_on;
        }
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == event::KeyEventKind::Press => {
                    app.on_key(key)?;
                }
                Event::Mouse(me) => app.on_mouse(me)?,
                _ => {}
            }
        }
    }
    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    // Mouse on by default (matches `App::mouse_on`); `M` toggles it off
    // at runtime when the user wants native text selection/copy back.
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn restore_terminal() -> Result<()> {
    disable_raw_mode()?;
    // Disabling capture when it's already off is a harmless no-op, so this
    // is safe on the panic path regardless of the runtime toggle state.
    execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen)?;
    Ok(())
}
