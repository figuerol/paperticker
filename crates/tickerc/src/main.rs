//! `tickerc` — ratatui terminal client for the paper-portfolio daemon.

mod app;
mod ui;
mod worker;

use std::io;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ticker_client::Client;

use app::App;

fn main() -> Result<()> {
    let sock = ticker_proto::default_socket_path();
    let client = Client::connect(&sock).with_context(|| {
        format!(
            "could not reach tickerd at {} — start it with `cargo run -p tickerd` (or the installed binary)",
            sock.display()
        )
    })?;
    // A second connection, for the worker. The protocol is one reply per
    // request on a socket, so sharing the app's would serialize exactly the
    // calls this is meant to get out of the way.
    let worker_client = Client::connect(&sock).with_context(|| {
        format!("opening a second connection to tickerd at {}", sock.display())
    })?;
    let mut app = App::new(client, worker::Worker::spawn(worker_client));
    app.boot()?;
    run_tui(&mut app)
}

/// Put the terminal back before a panic prints.
///
/// Without this a panic anywhere in the draw or event path leaves the user in
/// raw mode inside the alternate screen — no echo, no line editing, and the
/// backtrace scrolling somewhere they cannot see. Release builds are
/// `panic = "abort"`, so the hook is the only chance to run.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
        previous(info);
    }));
}

fn run_tui(app: &mut App) -> Result<()> {
    install_panic_hook();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
    terminal.show_cursor()?;
    result
}

fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> Result<()> {
    loop {
        app.pump_worker()?;
        terminal.draw(|f| ui::draw(f, app))?;
        app.frame = app.frame.wrapping_add(1);
        // Short enough that the spinner animates and progress lands promptly;
        // the loop redraws every pass regardless of whether a key arrived.
        if event::poll(Duration::from_millis(120))? {
            if let Event::Key(key) = event::read()? {
                // crossterm 0.28 emits both Press and Release on some terminals.
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                app.on_key(key)?;
            }
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}
