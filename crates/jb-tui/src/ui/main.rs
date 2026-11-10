//! Screen 3, the working screen: the column info + its backups (top),
//! the jailbreak settings, the log and the progress + flash button.

use super::{button, checkbox, log_pane};
use crate::app::{ActiveProgress, App, BackupSnapshot, Busy, Focus};
use jb_core::format::humansize;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Gauge, Paragraph, Wrap};

pub(super) fn render(f: &mut Frame, app: &mut App) {
    let [top, settings, log, bottom] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(f.area());

    // the column (left) and its backups (right)
    let [info, backups] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(top);
    render_info(f, app, info);
    render_backups(f, app, backups);

    // the settings
    let cols = Layout::horizontal([
        Constraint::Percentage(34),
        Constraint::Percentage(33),
        Constraint::Percentage(33),
    ])
    .split(settings);
    checkbox(
        f,
        app,
        cols[0],
        Focus::SetAdbd,
        "adbd (ADB по USB/TCP)",
        app.settings.adbd,
    );
    checkbox(
        f,
        app,
        cols[1],
        Focus::SetFormat,
        "форматировать раздел",
        app.settings.format_userdata,
    );
    checkbox(
        f,
        app,
        cols[2],
        Focus::SetNoBackup,
        "без бэкапа (с нуля)",
        app.settings.no_backup,
    );

    // the log, then the progress + the flash button
    log_pane(f, app, log);
    let [progress_area, button_area] =
        Layout::horizontal([Constraint::Min(10), Constraint::Length(16)]).areas(bottom);
    render_progress(f, app, progress_area);
    let idle = app.busy == Busy::None && !app.done;
    button(f, app, button_area, Focus::Flash, "ПРОШИТЬ", idle);
}

fn render_info(f: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();
    match &app.identity {
        Some(id) => {
            lines.push(Line::from(vec![
                Span::styled(
                    id.model.label(),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  ·  {}", id.key)),
            ]));
            if id.firstgen {
                lines.push(String::new().into());
                lines.push(Line::styled(
                    "! первое поколение: полный бэкап невозможен",
                    Style::default().fg(Color::Red),
                ));
                lines.push(Line::styled(
                    "  (BootROM залочен, выше 32 МиБ читается 0xCC)",
                    Style::default().fg(Color::Red),
                ));
            }
            for caveat in &id.caveats {
                lines.push(Line::styled(
                    format!("  {caveat}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
        None => lines.push(Line::from("нет данных")),
    }
    match app.busy {
        Busy::None => {}
        Busy::Backup => lines.push(Line::styled(
            "→ снимается бэкап…",
            Style::default().fg(Color::Cyan),
        )),
        Busy::Jailbreak => lines.push(Line::styled(
            "→ прошивается…",
            Style::default().fg(Color::Cyan),
        )),
        Busy::Restore => lines.push(Line::styled(
            "→ восстанавливается…",
            Style::default().fg(Color::Cyan),
        )),
    }
    if app.done {
        lines.push(Line::styled(
            "✓ готово — колонка перезагружается, можно закрывать",
            Style::default().fg(Color::Green),
        ));
    }
    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Колонка "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_backups(f: &mut Frame, app: &mut App, area: Rect) {
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(" Бэкапы этой колонки ");
    let inner_area = inner.inner(area);
    f.render_widget(inner, area);

    let [status, buttons] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(inner_area);
    let mut lines = Vec::new();
    // A full restore writes every INTACT image — even a caveated
    // capture (first-gen 0xCC, amlogic bad blocks) has restorable
    // partitions; the confirm modal lists what is skipped.
    let have_backup = app.backup.dir().is_some();
    match &app.backup {
        BackupSnapshot::Unknown => lines.push(Line::from("…")),
        BackupSnapshot::None => lines.push(Line::from("нет — снимется при прошивке")),
        BackupSnapshot::Verified { dir } => {
            lines.push(Line::styled(
                "✓ есть (проверен)",
                Style::default().fg(Color::Green),
            ));
            lines.push(Line::from(dir.display().to_string()));
        }
        BackupSnapshot::Unusable { dir, why } => {
            lines.push(Line::styled(
                "! с оговорками (часть образов нецелая)",
                Style::default().fg(Color::Yellow),
            ));
            lines.push(Line::from(dir.display().to_string()));
            for w in why {
                lines.push(Line::styled(
                    format!("  {w}"),
                    Style::default().fg(Color::Yellow),
                ));
            }
        }
    }
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), status);

    let idle = app.busy == Busy::None && !app.done;
    let have_dir = app.backup.dir().is_some();
    let rows: [Rect; 3] = Layout::vertical([Constraint::Length(1); 3]).areas(buttons);
    button(
        f,
        app,
        rows[0],
        Focus::TakeBackup,
        "Снять бэкап",
        idle && !have_backup,
    );
    button(
        f,
        app,
        rows[1],
        Focus::Restore,
        "Восстановить",
        idle && have_backup,
    );
    button(
        f,
        app,
        rows[2],
        Focus::DeleteBackup,
        "Удалить бэкап",
        idle && have_dir,
    );
}

fn render_progress(f: &mut Frame, app: &App, area: Rect) {
    match &app.progress {
        None => f.render_widget(
            Paragraph::new("").block(Block::default().borders(Borders::ALL)),
            area,
        ),
        Some(p) => {
            let (label, ratio) = progress_label(p);
            f.render_widget(
                Gauge::default()
                    .block(Block::default().borders(Borders::ALL))
                    .label(label)
                    .ratio(ratio.clamp(0.0, 1.0)),
                area,
            );
        }
    }
}

fn progress_label(p: &ActiveProgress) -> (String, f64) {
    let ratio = if p.total == 0 {
        0.0
    } else {
        p.done as f64 / p.total as f64
    };
    // sectors vs bytes: the amlogic streams are byte-oriented, the
    // rockchip reads are sector counts — humansize reads fine for both
    (format!("{}: {}", p.label, humansize(p.done)), ratio)
}
