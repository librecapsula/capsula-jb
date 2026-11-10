//! Screen between "a device is on the bus" and "connected": the worker
//! is bringing the column up (a loader download into RAM, driver
//! installs — potentially tens of seconds). No controls: the live
//! bring-up log is the content; Esc leaves the tool (nothing is being
//! written yet).

use super::log_pane;
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let [head, log] = Layout::vertical([Constraint::Length(5), Constraint::Min(3)]).areas(f.area());

    let title = format!(" Подготавливаем {}: … ", app.selected_model.label());
    let text = vec![
        Line::from(vec![
            Span::styled("Устройство найдено", Style::default().fg(Color::Green)),
            Span::raw(" — поднимаю рабочий режим"),
        ]),
        Line::from("загружаю лоадер в RAM / ставлю драйверы, это может занять полминуты"),
    ];
    f.render_widget(
        Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).title(title))
            .style(Style::default().add_modifier(Modifier::DIM))
            .wrap(Wrap { trim: false }),
        head,
    );
    log_pane(f, app, log);
}
