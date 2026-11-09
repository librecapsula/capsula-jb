//! Rendering. Every frame also refreshes `app.hit` — the rects of the
//! focusable controls — so the mouse can hit-test clicks against
//! exactly what is on screen.

use crate::app::{ActiveProgress, App, BackupSnapshot, Busy, Confirm, Focus, Screen};
use jb_core::format::humansize;
use jb_core::session::Model;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};

pub fn render(f: &mut Frame, app: &mut App) {
    app.hit.clear();
    match app.screen {
        Screen::ModelSelect => render_model_select(f, app),
        Screen::Waiting => render_waiting(f, app),
        Screen::Main => render_main(f, app),
    }
    if app.confirm.is_some() {
        render_modal(f, app);
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

/// 1: the model radio + the connection instructions of the selection.
fn render_model_select(f: &mut Frame, app: &mut App) {
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

/// 2: the instructions again + the live log tail (bring-up steps).
fn render_waiting(f: &mut Frame, app: &mut App) {
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

/// 3: the working screen.
fn render_main(f: &mut Frame, app: &mut App) {
    let [top, settings, log, bottom] = Layout::vertical([
        Constraint::Length(8),
        Constraint::Length(5),
        Constraint::Min(4),
        Constraint::Length(3),
    ])
    .areas(f.area());

    // 3.2: the column (left) and its backups (right)
    let [info, backups] =
        Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)]).areas(top);
    render_info(f, app, info);
    render_backups(f, app, backups);

    // 3.3: the settings
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

    // 3.4: the log, then the progress + the flash button
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

/// A settings checkbox line.
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

/// The modal: a write confirmation (with the plan) or a delete
/// confirmation.
fn render_modal(f: &mut Frame, app: &mut App) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use jb_core::session::Session;

    fn app() -> App {
        // no worker needed for render tests: events are never drained
        struct NoBackend;
        impl jb_core::backend::Backend for NoBackend {
            fn name(&self) -> &'static str {
                "none"
            }
            fn present(&self) -> anyhow::Result<bool> {
                Ok(false)
            }
            fn connect(
                &mut self,
                _w: Option<std::time::Duration>,
                _p: jb_core::progress::ProgressFn,
            ) -> anyhow::Result<jb_core::session::Identity> {
                unreachable!()
            }
            fn platform(&mut self) -> &mut dyn jb_core::scenario::Platform {
                unreachable!()
            }
        }
        impl jb_core::scenario::Platform for NoBackend {
            fn ensure_backup(
                &mut self,
                _n: bool,
                _p: jb_core::progress::ProgressFn,
            ) -> anyhow::Result<jb_core::scenario::Prepared> {
                unreachable!()
            }
            fn backup_state(&self) -> jb_core::scenario::BackupState {
                jb_core::scenario::BackupState::None
            }
            fn data_size(&mut self, _p: jb_core::progress::ProgressFn) -> anyhow::Result<u64> {
                unreachable!()
            }
            fn data_partition(&self) -> &'static str {
                ""
            }
            fn describe_write(&self, _f: bool, _n: bool) -> String {
                String::new()
            }
            fn describe_restore(&self) -> String {
                String::new()
            }
            fn build_data_image(
                &self,
                _b: Option<&std::path::Path>,
                _o: &std::path::Path,
                _s: u64,
                _f: &[jb_core::overlay::OverlayFile],
                _p: jb_core::progress::ProgressFn,
            ) -> anyhow::Result<()> {
                unreachable!()
            }
            fn write_data_image(
                &mut self,
                _i: &std::path::Path,
                _p: jb_core::progress::ProgressFn,
            ) -> anyhow::Result<()> {
                unreachable!()
            }
            fn restore_set(
                &mut self,
            ) -> anyhow::Result<(Vec<jb_core::scenario::RestoreItem>, Vec<String>)> {
                unreachable!()
            }
            fn write_restore(
                &mut self,
                _items: &[jb_core::scenario::RestoreItem],
                _p: jb_core::progress::ProgressFn,
            ) -> anyhow::Result<()> {
                unreachable!()
            }
            fn finish(&mut self, _p: jb_core::progress::ProgressFn) -> anyhow::Result<()> {
                unreachable!()
            }
            fn report_backup(&self, _p: &jb_core::scenario::Prepared) {}
        }
        App::new(Session::spawn(vec![Box::new(NoBackend)], None))
    }

    #[test]
    fn hit_rects_cover_the_focusable_controls() {
        let mut a = app();
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        // model select: two model rows + the next button
        assert!(a.hit.contains_key(&Focus::Model(0)));
        assert!(a.hit.contains_key(&Focus::Model(1)));
        assert!(a.hit.contains_key(&Focus::ModelNext));

        // the main screen: buttons + checkboxes + the flash button
        a.screen = crate::app::Screen::Main;
        a.identity = Some(jb_core::session::Identity {
            model: Model::Capsule,
            key: "SN".into(),
            firstgen: false,
            caveats: vec![],
        });
        terminal.draw(|f| render(f, &mut a)).unwrap();
        for f in [
            Focus::TakeBackup,
            Focus::Restore,
            Focus::DeleteBackup,
            Focus::SetAdbd,
            Focus::SetFormat,
            Focus::SetNoBackup,
            Focus::Flash,
        ] {
            assert!(a.hit.contains_key(&f), "нет rect для {f:?}");
        }
        // the log rect is registered for the wheel
        assert!(a.log_rect.height > 0);
    }

    #[test]
    fn click_on_flash_button_focuses_and_activates() {
        let mut a = app();
        a.screen = crate::app::Screen::Main;
        a.identity = Some(jb_core::session::Identity {
            model: Model::Capsule,
            key: "SN".into(),
            firstgen: false,
            caveats: vec![],
        });
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let rect = a.hit[&Focus::Flash];
        let mid = (rect.x + rect.width / 2, rect.y);
        a.on_mouse(ratatui::crossterm::event::MouseEvent {
            kind: ratatui::crossterm::event::MouseEventKind::Down(
                ratatui::crossterm::event::MouseButton::Left,
            ),
            column: mid.0,
            row: mid.1,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        });
        assert_eq!(a.focus, Focus::Flash);
        assert_eq!(a.busy, Busy::Jailbreak);
        a.on_session_event(jb_core::session::Event::Finished(Ok(())));
        assert_eq!(a.busy, Busy::None);
    }

    #[test]
    fn wheel_over_log_scrolls() {
        let mut a = app();
        a.screen = crate::app::Screen::Main;
        a.identity = Some(jb_core::session::Identity {
            model: Model::Capsule,
            key: "SN".into(),
            firstgen: false,
            caveats: vec![],
        });
        for i in 0..100 {
            a.push_log(format!("line {i}"));
        }
        let backend = ratatui::backend::TestBackend::new(100, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| render(f, &mut a)).unwrap();
        let (x, y) = (a.log_rect.x + 2, a.log_rect.y + 2);
        let mk = |kind| ratatui::crossterm::event::MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        };
        assert_eq!(a.log_scroll, 0);
        a.on_mouse(mk(ratatui::crossterm::event::MouseEventKind::ScrollUp));
        assert_eq!(a.log_scroll, 1);
        a.on_mouse(mk(ratatui::crossterm::event::MouseEventKind::ScrollDown));
        assert_eq!(a.log_scroll, 0);
    }
}
