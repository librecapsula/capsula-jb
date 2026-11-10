//! Screen 1: the model menu — arrows move the cursor, Enter/click
//! chooses the model and shows the connection instructions on the
//! next screen. The rows come from the model registry (`Model::ALL`,
//! in order); the row index maps onto `Focus::Model(i)` (see
//! [`crate::app`]'s input routing). Над меню — ascii-заставка
//! [`BANNER`]; узкий или низкий терминал остаётся без неё.

use super::centered_rect;
use crate::app::{App, Focus};
use jb_core::model::Model;
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

/// Заставка над меню — «capsula jailbreak» одной строкой, фиглет
/// standard. Все строки добиты до одной ширины, чтобы блок
/// центрировался как целое, а не построчно (построчное центрирование
/// разъехало бы верхние акценты букв). Терминал уже 77 колонок или
/// ниже 12 строк заставку не получает — рисовать её обрезанной хуже,
/// чем остаться без украшения.
const BANNER: [&str; 6] = [
    r#"                           _           _       _ _ _                    _    "#,
    r#"  ___ __ _ _ __  ___ _   _| | __ _    (_) __ _(_) | |__  _ __ ___  __ _| | __"#,
    r#" / __/ _` | '_ \/ __| | | | |/ _` |   | |/ _` | | | '_ \| '__/ _ \/ _` | |/ /"#,
    r#"| (_| (_| | |_) \__ \ |_| | | (_| |   | | (_| | | | |_) | | |  __/ (_| |   < "#,
    r#" \___\__,_| .__/|___/\__,_|_|\__,_|  _/ |\__,_|_|_|_.__/|_|  \___|\__,_|_|\_|"#,
    r#"          |_|                       |__/                                     "#,
];

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let banner_height = BANNER.len() as u16;
    let banner_width = BANNER[0].len() as u16;
    let rest = if area.width >= banner_width && area.height >= banner_height + 6 {
        let banner_rect = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: banner_height,
        };
        let lines: Vec<Line> = BANNER.iter().map(|l| Line::from(*l)).collect();
        f.render_widget(
            Paragraph::new(lines)
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::DarkGray)),
            banner_rect,
        );
        Rect {
            x: area.x,
            y: area.y + banner_height,
            width: area.width,
            height: area.height - banner_height,
        }
    } else {
        area
    };
    let card = centered_rect(rest, 50, 4);

    let mut lines = Vec::new();
    for (i, model) in Model::ALL.iter().enumerate() {
        let focused = app.focus == Focus::Model(i);
        let cursor = if focused { "❯" } else { " " };
        let style = if focused {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::styled(format!(" {cursor} {}", model.label()), style));
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
