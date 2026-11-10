//! Rendering. Every frame also refreshes `app.hit` — the rects of the
//! focusable controls — so the mouse can hit-test clicks against
//! exactly what is on screen. One module per screen: [`model_select`],
//! [`waiting`], [`main`], plus the [`modal`] overlay; the tests are in
//! [`tests`].

mod main;
mod modal;
mod model_select;
#[cfg(test)]
mod tests;
mod waiting;

use crate::app::{App, Focus, Screen};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

pub fn render(f: &mut Frame, app: &mut App) {
    app.hit.clear();
    match app.screen {
        Screen::ModelSelect => model_select::render(f, app),
        Screen::Waiting => waiting::render(f, app),
        Screen::Main => main::render(f, app),
    }
    if app.confirm.is_some() {
        modal::render(f, app);
    }
    // the transient status line (a notice or the last error)
    let status: Option<String> = match (&app.notice, &app.error) {
        (Some(n), _) => Some(n.clone()),
        (None, Some(e)) => Some(format!("ошибка: {e}")),
        (None, None) => None,
    };
    if let Some(status) = status {
        let area = status_line_rect(f.area());
        f.render_widget(Clear, area);
        let color = if app.error.is_some() {
            Color::Red
        } else {
            Color::Yellow
        };
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(color)),
            area,
        );
    }
}

fn status_line_rect(area: Rect) -> Rect {
    Rect {
        x: area.x,
        y: area.bottom().saturating_sub(1),
        width: area.width,
        height: 1,
    }
}

/// The log tail with scroll-from-bottom.
fn log_pane(f: &mut Frame, app: &mut App, area: Rect) {
    app.log_rect = area;
    let block = Block::default().borders(Borders::ALL).title(" Лог ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width < 4 {
        return;
    }
    let rows = inner.height as usize;
    let total = app.logs.len();
    let skip = total.saturating_sub(rows + app.log_scroll as usize);
    let visible: Vec<Line> = app
        .logs
        .iter()
        .skip(skip)
        .take(rows)
        .map(|l| Line::from(l.as_str()))
        .collect();
    f.render_widget(Paragraph::new(visible), inner);
}

/// A focusable button; disabled ones render dim and eat the click.
fn button(f: &mut Frame, app: &mut App, area: Rect, focus: Focus, text: &str, enabled: bool) {
    app.hit.insert(focus, area);
    let style = if !enabled {
        Style::default().fg(Color::DarkGray)
    } else if app.focus == focus {
        Style::default()
            .fg(Color::Black)
            .bg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    f.render_widget(
        Paragraph::new(format!(" ▐ {text} ▌ "))
            .alignment(Alignment::Center)
            .style(style),
        area,
    );
}

fn checkbox(f: &mut Frame, app: &mut App, area: Rect, focus: Focus, label: &str, checked: bool) {
    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: 1,
    };
    app.hit.insert(focus, inner);
    let mark = if checked { "[x]" } else { "[ ]" };
    let style = if app.focus == focus {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    f.render_widget(
        Paragraph::new(format!("{mark} {label}"))
            .style(style)
            .wrap(Wrap { trim: false }),
        inner,
    );
    f.render_widget(Block::default().borders(Borders::ALL), area);
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let w = area.width.saturating_mul(percent_x) / 100;
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width.saturating_sub(w)) / 2,
        y: area.y + (area.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}
