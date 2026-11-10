//! Input routing: the focus ring, the keyboard/mouse handling and the
//! control activation.

use super::{App, BackupSnapshot, Busy, Confirm, Screen};
use jb_core::model::Model;
use jb_core::session::Session;

/// A focusable control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    Model(usize),
    TakeBackup,
    Restore,
    DeleteBackup,
    SetAdbd,
    SetFormat,
    SetNoBackup,
    Flash,
    ModalOk,
    ModalCancel,
}

impl Focus {
    /// The focus cycle order of the current screen. The model rows
    /// follow the registry (`Model::ALL`), the rest are fixed.
    fn ring(app: &App) -> Vec<Focus> {
        const MAIN: &[Focus] = &[
            Focus::TakeBackup,
            Focus::Restore,
            Focus::DeleteBackup,
            Focus::SetAdbd,
            Focus::SetFormat,
            Focus::SetNoBackup,
            Focus::Flash,
        ];
        const MODAL: &[Focus] = &[Focus::ModalOk, Focus::ModalCancel];
        match app.confirm {
            Some(_) => MODAL.to_vec(),
            None => match app.screen {
                Screen::ModelSelect => (0..Model::ALL.len()).map(Focus::Model).collect(),
                _ => MAIN.to_vec(),
            },
        }
    }

    fn next(app: &App, backwards: bool) -> Focus {
        let ring = Self::ring(app);
        let pos = ring.iter().position(|f| *f == app.focus).unwrap_or(0);
        let delta = if backwards {
            ring.len().wrapping_sub(1)
        } else {
            1
        };
        ring[(pos + delta) % ring.len()]
    }
}

impl App {
    pub(crate) fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
        use ratatui::crossterm::event::KeyCode;

        if self.confirm.is_some() {
            match key.code {
                KeyCode::Enter => self.activate(self.focus),
                KeyCode::Tab | KeyCode::BackTab => {
                    self.focus = Focus::next(self, key.code == KeyCode::BackTab)
                }
                KeyCode::Left | KeyCode::Up => self.focus = Focus::ModalOk,
                KeyCode::Right | KeyCode::Down => self.focus = Focus::ModalCancel,
                KeyCode::Esc | KeyCode::Char('n') => self.cancel_confirm(),
                KeyCode::Char('y') => {
                    self.focus = Focus::ModalOk;
                    self.activate(Focus::ModalOk)
                }
                _ => {}
            }
            return;
        }

        match key.code {
            KeyCode::Tab => self.focus = Focus::next(self, false),
            KeyCode::BackTab => self.focus = Focus::next(self, true),
            KeyCode::Up | KeyCode::Left => {
                // Arrows always move the focus ring; the wheel scrolls
                // the log.
                self.focus = Focus::next(self, true)
            }
            KeyCode::Down | KeyCode::Right => self.focus = Focus::next(self, false),
            KeyCode::Enter | KeyCode::Char(' ') => self.activate(self.focus),
            KeyCode::Esc => self.escape(),
            KeyCode::Char('q') | KeyCode::Char('Q') => self.request_quit(),
            // In raw mode Ctrl+C is not a SIGINT — it arrives as a key.
            // Handle it like q: IDE "Stop" buttons (RustRover et al.)
            // send Ctrl+C into the emulated terminal instead of a
            // signal, so without this the process survives Stop.
            KeyCode::Char('c')
                if key
                    .modifiers
                    .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
            {
                self.request_quit()
            }
            _ => {}
        }
    }

    pub(crate) fn on_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
        use ratatui::crossterm::event::MouseEventKind;
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                if self.contains(self.log_rect, point) {
                    self.log_scroll = self.log_scroll.saturating_add(1);
                }
            }
            MouseEventKind::ScrollDown => {
                if self.contains(self.log_rect, point) {
                    self.log_scroll = self.log_scroll.saturating_sub(1);
                }
            }
            MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left) => {
                if self.confirm.is_some() && self.contains(self.modal_rect(), point) {
                    for (f, r) in self.hit.iter() {
                        if matches!(f, Focus::ModalOk | Focus::ModalCancel)
                            && self.contains(*r, point)
                        {
                            self.focus = *f;
                            self.activate(*f);
                            return;
                        }
                    }
                    return;
                }
                for (f, r) in self.hit.iter() {
                    if self.contains(*r, point) {
                        self.focus = *f;
                        self.activate(*f);
                        return;
                    }
                }
            }
            _ => {}
        }
    }

    fn contains(&self, r: ratatui::layout::Rect, (x, y): (u16, u16)) -> bool {
        x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
    }

    fn modal_rect(&self) -> ratatui::layout::Rect {
        // updated by the renderer alongside `hit`
        self.hit
            .get(&Focus::ModalOk)
            .copied()
            .map(|r| ratatui::layout::Rect {
                x: r.x.saturating_sub(6),
                y: r.y.saturating_sub(6),
                width: r.width.saturating_add(12),
                height: r.height.saturating_add(12),
            })
            .unwrap_or_default()
    }

    fn escape(&mut self) {
        match self.screen {
            Screen::ModelSelect => self.quit = true,
            // Changing the model = a full restart of the tool; the
            // waiting screen only exits.
            Screen::Waiting | Screen::Main => self.request_quit(),
        }
    }

    fn request_quit(&mut self) {
        if self.busy != Busy::None {
            self.notice = Some(
                "идёт операция — дождитесь завершения (запись нельзя \
                 прерывать)"
                    .into(),
            );
            return;
        }
        // an unanswered confirm would block the worker forever —
        // answer it on the way out
        self.cancel_confirm();
        self.quit = true;
    }

    pub(crate) fn activate(&mut self, focus: Focus) {
        match focus {
            Focus::Model(i) if self.screen == Screen::ModelSelect => {
                self.selected_model = Model::ALL[i];
                // The worker starts here — the chosen model gates the
                // platform, its backend is the only one polled.
                self.session = Some(Session::spawn(
                    vec![(self.for_model)(self.selected_model)],
                    None,
                ));
                self.go_waiting();
            }
            Focus::SetAdbd => self.settings.adbd = !self.settings.adbd,
            Focus::SetFormat => self.settings.format_userdata = !self.settings.format_userdata,
            Focus::SetNoBackup => self.settings.no_backup = !self.settings.no_backup,
            Focus::TakeBackup => {
                if self.busy == Busy::None && !self.done {
                    self.busy = Busy::Backup;
                    self.notice = None;
                    self.error = None;
                    self.session().command(jb_core::session::Command::Backup);
                }
            }
            Focus::Restore => {
                if self.busy == Busy::None && !self.done && self.backup.dir().is_some() {
                    self.busy = Busy::Restore;
                    self.notice = None;
                    self.error = None;
                    self.session().command(jb_core::session::Command::Restore);
                }
            }
            Focus::DeleteBackup => {
                if self.busy == Busy::None
                    && let Some(dir) = self.backup.dir()
                {
                    self.confirm = Some(Confirm::DeleteBackup {
                        dir: dir.to_path_buf(),
                    });
                    self.focus = Focus::ModalOk;
                }
            }
            Focus::Flash => {
                if self.busy == Busy::None && !self.done {
                    self.busy = Busy::Jailbreak;
                    self.notice = None;
                    self.error = None;
                    self.session()
                        .command(jb_core::session::Command::Jailbreak(self.settings.params()));
                }
            }
            Focus::ModalOk => match self.confirm.take() {
                Some(Confirm::Write { reply, .. }) => {
                    if let Some(reply) = reply {
                        let _ = reply.send(jb_core::session::Decision::Proceed);
                    }
                    self.focus = Focus::Flash;
                }
                Some(Confirm::DeleteBackup { dir }) => {
                    match std::fs::remove_dir_all(&dir) {
                        Ok(()) => {
                            self.backup = BackupSnapshot::None;
                            self.notice = Some(format!("бэкап удалён: {}", dir.display()));
                        }
                        Err(e) => self.error = Some(format!("удаление бэкапа: {e:#}")),
                    }
                    self.focus = Focus::DeleteBackup;
                }
                None => {}
            },
            Focus::ModalCancel => self.cancel_confirm(),
            Focus::Model(_) => {}
        }
    }

    /// Move to the waiting screen and show the connection
    /// instructions of the selected model. The focus stays on a model
    /// row — inert there (activation is guarded by ModelSelect), Tab
    /// walks into the main ring.
    fn go_waiting(&mut self) {
        self.screen = Screen::Waiting;
        self.hints = vec![crate::hint_text(self.selected_model).to_string()];
    }

    /// Answer the write confirm with Abort (or just drop a local
    /// modal).
    fn cancel_confirm(&mut self) {
        if let Some(Confirm::Write { reply, .. }) = self.confirm.take()
            && let Some(reply) = reply
        {
            let _ = reply.send(jb_core::session::Decision::Abort);
        }
        if self.screen == Screen::Main {
            self.focus = Focus::Flash;
        }
    }
}
