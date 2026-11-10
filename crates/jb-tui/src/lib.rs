//! capsula-jb — the terminal UI: opens when the tool is launched
//! without a single argument in an interactive terminal (the branch in
//! jb-cli/src/main.rs). The model menu comes first; the session worker
//! starts only when the model is committed (it polls exactly that
//! model's backend) and the waiting screen then shows the connection
//! instructions. Changing the model = restarting the tool.
//!
//! The log pane is fed by the `log` facade (see [`logger`]) — domain
//! crates keep logging as usual.

mod app;
mod logger;
mod ui;

use anyhow::Result;
use jb_core::backend::Backend;
use jb_core::model::Model;
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

/// Launch the UI; `for_model` is the binary's platform dispatch (see
/// jb-cli's `backend`). Returns when the user quits.
pub fn run(for_model: fn(Model) -> Box<dyn Backend>) -> Result<()> {
    let (log_tx, log_rx) = mpsc::channel::<String>();
    logger::install(log_tx);

    let mut app = app::App::new(for_model);

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
        // next input poll blocks for the tick (a no-op before the
        // model commit — there is no worker yet)
        app.poll_events();
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
pub(crate) fn hint_text(model: Model) -> String {
    jb_core::config::Config::embedded()
        .connect_text(model)
        .to_owned()
}
