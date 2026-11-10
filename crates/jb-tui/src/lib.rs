//! capsula-jb — the terminal UI: opens when the tool is launched
//! without a single argument in an interactive terminal (the branch in
//! jb-cli/src/main.rs). Drives the same
//! [`jb_core::session::Session`] worker as the CLI; the model select
//! screen is instructions only — the session accepts either model and
//! switches on the connected identity.
//!
//! The log pane is fed by the `log` facade (see [`logger`]) — domain
//! crates keep logging as usual.

mod app;
mod logger;
mod ui;

use anyhow::Result;
use jb_core::backend::Backend;
use jb_core::session::Session;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::sync::mpsc;
use std::time::Duration;

/// The poll tick: how often the session/log channels are drained even
/// without input.
const TICK: Duration = Duration::from_millis(50);

/// Launch the UI over the given backends; returns when the user quits.
pub fn run(backends: Vec<Box<dyn Backend>>) -> Result<()> {
    let (log_tx, log_rx) = mpsc::channel::<String>();
    logger::install(log_tx);

    let session = Session::spawn(backends, None);
    let mut app = app::App::new(session);

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;

    let result = event_loop(&mut terminal, &mut app, &log_rx);

    restore_terminal();
    logger::detach();
    result
}

/// One tick of the loop: drain the worker and the log, handle input,
/// redraw.
fn event_loop(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut app::App,
    log_rx: &mpsc::Receiver<String>,
) -> Result<()> {
    loop {
        // worker events first: a confirm must be answerable before the
        // next input poll blocks for the tick
        while let Ok(ev) = app.session.events().try_recv() {
            app.on_session_event(ev);
        }
        while let Ok(line) = log_rx.try_recv() {
            app.push_log(line);
        }
        if event::poll(TICK)? {
            match event::read()? {
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                TermEvent::Mouse(mouse) => app.on_mouse(mouse),
                TermEvent::Resize(_, _) => {}
                _ => {}
            }
        }
        if app.quit {
            return Ok(());
        }
        terminal.draw(|f| ui::render(f, app))?;
    }
}

fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
}

/// The connection instructions of a model, from the embedded config —
/// the same texts the CLI prints as wait hints.
pub(crate) fn hint_text(model: jb_core::session::Model) -> String {
    let config = jb_core::config::Config::embedded();
    match model {
        jb_core::session::Model::Capsule => config.hint.text.clone(),
        jb_core::session::Model::CapsuleMini => config.hint.amlogic_text.clone(),
    }
}
