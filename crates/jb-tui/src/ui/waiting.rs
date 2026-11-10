//! Screen 2: the instructions again + the live log tail (bring-up
//! steps).

use super::log_pane;
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let [hint, log] =
        Layout::vertical([Constraint::Length(14), Constraint::Min(3)]).areas(f.area());

    let title = format!(
        " Ждём {}: {} ",
        app.selected_model.label(),
        "подключения по USB"
    );
    f.render_widget(
        Paragraph::new(app.hints.join("\n\n"))
            .block(Block::default().borders(Borders::ALL).title(title))
            .wrap(Wrap { trim: false }),
        hint,
    );
    log_pane(f, app, log);
}
