//! The modal overlay: a write confirmation (with the plan), a
//! first-generation notice (a single Continue button) or a delete
//! confirmation.

use super::{button, centered_rect};
use crate::app::{App, Confirm, Focus};
use jb_core::format::humansize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

/// What the modal shows: its title, body, the OK button label, and
/// whether the cancel button exists (the first-gen notice is
/// Continue-only — Esc aborts the run instead). Owned: the renderer
/// mutates `app` (the button hit-rects) while drawing the body.
type Modal = (&'static str, Text<'static>, &'static str, bool);

fn modal(app: &App) -> Option<Modal> {
    match app.confirm.as_ref() {
        Some(Confirm::Write { plan, .. }) => {
            let verb = if plan.restore {
                "восстановление"
            } else {
                "запись"
            };
            Some((
                " Подтверждение ",
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
                false,
            ))
        }
        Some(Confirm::Firstgen { notice, .. }) => Some((
            " Первое поколение ",
            Text::from(
                notice
                    .iter()
                    .map(|l| Line::from(l.clone()))
                    .collect::<Vec<_>>(),
            ),
            "Продолжить",
            true,
        )),
        Some(Confirm::DeleteBackup { dir }) => Some((
            " Подтверждение ",
            Text::from(vec![
                Line::from("Удалить бэкап этой колонки?"),
                Line::from(dir.display().to_string()),
                String::new().into(),
                Line::from("Его можно будет снять заново."),
            ]),
            "Удалить",
            false,
        )),
        None => None,
    }
}

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let Some((title, text, ok_label, single_button)) = modal(app) else {
        return;
    };
    let area = centered_rect(f.area(), 64, 14);
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let [text_area, buttons] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(inner);
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), text_area);

    if single_button {
        button(f, app, buttons, Focus::ModalOk, ok_label, true);
    } else {
        let [ok_area, cancel_area] =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .areas(buttons);
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
}
