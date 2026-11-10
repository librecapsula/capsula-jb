//! Screen 1: the model radio + the connection instructions of the
//! selection.

use super::button;
use crate::app::{App, Focus};
use jb_core::session::Model;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let [list, hint] =
        Layout::vertical([Constraint::Length(6), Constraint::Min(1)]).areas(f.area());

    let models = [
        (Model::Capsule, "Большая Капсула (USB 2207:…)"),
        (
            Model::CapsuleMini,
            "Капсула Мини, поздние ревизии (USB 1b8e:c003)",
        ),
    ];
    let mut lines = Vec::new();
    for (i, (model, label)) in models.iter().enumerate() {
        let selected = app.selected_model == *model;
        let focused = app.focus == Focus::Model(i);
        let mark = if selected { "(•)" } else { "( )" };
        let style = if focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!(" {mark} {label}"), style));
        app.hit.insert(
            Focus::Model(i),
            Rect {
                x: list.x,
                y: list.y + 1 + i as u16,
                width: list.width,
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
        list,
    );
    button(
        f,
        app,
        Rect {
            x: list.x + 2,
            y: list.bottom().saturating_sub(2),
            width: 12,
            height: 1,
        },
        Focus::ModelNext,
        "Далее ↵",
        true,
    );

    f.render_widget(
        Paragraph::new(crate::hint_text(app.selected_model))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Как подключить "),
            )
            .wrap(Wrap { trim: false }),
        hint,
    );
}
