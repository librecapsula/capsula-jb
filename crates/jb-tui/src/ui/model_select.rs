//! Screen 1: the model menu — arrows move the cursor, Enter/click
//! chooses the model and shows the connection instructions on the
//! next screen. The row order maps onto `Focus::Model(i)` (see
//! [`crate::app`]'s input routing).

use super::centered_rect;
use crate::app::{App, Focus};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let card = centered_rect(f.area(), 50, 4);

    let labels = ["Капсула (Большая)", "Капсула Мини (Amlogic)"];
    let mut lines = Vec::new();
    for (i, label) in labels.iter().enumerate() {
        let focused = app.focus == Focus::Model(i);
        let cursor = if focused { "❯" } else { " " };
        let style = if focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!(" {cursor} {label}"), style));
        app.hit.insert(
            Focus::Model(i),
            Rect {
                x: card.x,
                y: card.y + 1 + i as u16,
                width: card.width,
                height: 1,
            },
        );
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Выберите колонку "),
        ),
        card,
    );
}
