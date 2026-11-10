//! The modal overlay: a write confirmation (with the plan) or a delete
//! confirmation.

use super::{button, centered_rect};
use crate::app::{App, Confirm, Focus};
use jb_core::format::humansize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let area = centered_rect(f.area(), 64, 14);
    f.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Подтверждение ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let (text, ok_label) = match app.confirm.as_ref() {
        Some(Confirm::Write { plan, .. }) => {
            let verb = if plan.restore {
                "восстановление"
            } else {
                "запись"
            };
            (
                Text::from(vec![
                    Line::from(vec![
                        Span::styled(format!("{verb} "), Style::default().fg(Color::Red)),
                        Span::styled(
                            plan.partition.to_string(),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(" ({})", humansize(plan.size))),
                    ]),
                    Line::from(format!("{}.", plan.mode)),
                    String::new().into(),
                    Line::from("Ничего не записано — отказ безопасен."),
                ]),
                if plan.restore {
                    "Восстановить"
                } else {
                    "Записать"
                },
            )
        }
        Some(Confirm::DeleteBackup { dir }) => (
            Text::from(vec![
                Line::from("Удалить бэкап этой колонки?"),
                Line::from(dir.display().to_string()),
                String::new().into(),
                Line::from("Его можно будет снять заново."),
            ]),
            "Удалить",
        ),
        None => return,
    };

    let [text_area, buttons] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), text_area);

    let [ok_area, cancel_area] =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(buttons);
    button(f, app, ok_area, Focus::ModalOk, ok_label, true);
    button(
        f,
        app,
        cancel_area,
        Focus::ModalCancel,
        "Отмена (Esc)",
        true,
    );
}
