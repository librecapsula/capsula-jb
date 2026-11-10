//! The TUI application state: the screen machine (model select →
//! waiting → main), the busy/confirm modals, and the input routing.
//! Plain data + methods, testable without a terminal.

use jb_core::progress::Progress;
use jb_core::scenario::{Params, WritePlan};
use jb_core::session::{Command, Decision, Event, Identity, Model, Session};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Log lines kept in the pane.
const LOG_CAP: usize = 2000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    ModelSelect,
    Waiting,
    Main,
}

/// The jailbreak settings; maps 1:1 onto
/// [`jb_core::scenario::Params`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub adbd: bool,
    pub format_userdata: bool,
    pub no_backup: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            adbd: true,
            format_userdata: false,
            no_backup: false,
        }
    }
}

impl Settings {
    fn params(&self) -> Params {
        Params {
            format_userdata: self.format_userdata,
            no_backup: self.no_backup,
            adbd: self.adbd,
        }
    }
}

/// The operation running on the session worker (buttons are disabled
/// while set — a write must not be interrupted).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Busy {
    None,
    Jailbreak,
    Backup,
    Restore,
}

/// The connected column's backup situation (mirrored from
/// [`jb_core::scenario::BackupState`] with the directory kept for
/// delete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupSnapshot {
    Unknown,
    None,
    Verified {
        dir: PathBuf,
    },
    Unusable {
        dir: PathBuf,
        why: Vec<String>,
    },
}

impl BackupSnapshot {
    pub fn dir(&self) -> Option<&Path> {
        match self {
            BackupSnapshot::Verified { dir } | BackupSnapshot::Unusable { dir, .. } => Some(dir),
            _ => None,
        }
    }
}

use std::path::Path;

/// The active progress line.
#[derive(Debug, Clone)]
pub struct ActiveProgress {
    pub label: String,
    pub done: u64,
    pub total: u64,
}

/// A focusable control.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Focus {
    Model(usize),
    ModelNext,
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
    /// The focus cycle order of the current screen.
    fn ring(app: &App) -> &'static [Focus] {
        const MODEL: &[Focus] = &[Focus::Model(0), Focus::Model(1), Focus::ModelNext];
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
            Some(_) => MODAL,
            None => match app.screen {
                Screen::ModelSelect => MODEL,
                _ => MAIN,
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

#[derive(Debug, Clone)]
pub enum Confirm {
    /// The scenario asks to allow the write (the worker is BLOCKED
    /// until the reply).
    Write {
        plan: WritePlan,
        reply: Option<Sender<Decision>>,
    },
    /// Local: delete the backup directory.
    DeleteBackup { dir: PathBuf },
}

pub struct App {
    pub session: Session,
    pub screen: Screen,
    pub quit: bool,

    pub selected_model: Model,

    pub hints: Vec<String>,

    pub identity: Option<Identity>,
    pub backup: BackupSnapshot,
    pub settings: Settings,
    pub busy: Busy,
    /// A jailbreak/restore finished OK — the column is rebooting, no
    /// further operations make sense until a reconnect.
    pub done: bool,
    pub progress: Option<ActiveProgress>,

    pub confirm: Option<Confirm>,
    pub error: Option<String>,
    pub notice: Option<String>,

    pub logs: VecDeque<String>,
    /// Lines scrolled up from the tail (0 = follow the output).
    pub log_scroll: u16,

    pub focus: Focus,
    /// Rendered rects of the focusable controls, refreshed every frame;
    /// the mouse hit-tests against them.
    pub hit: std::collections::HashMap<Focus, ratatui::layout::Rect>,
    pub log_rect: ratatui::layout::Rect,
}

impl App {
    pub fn new(session: Session) -> Self {
        App {
            session,
            screen: Screen::ModelSelect,
            quit: false,
            selected_model: Model::Capsule,
            hints: Vec::new(),
            identity: None,
            backup: BackupSnapshot::Unknown,
            settings: Settings::default(),
            busy: Busy::None,
            done: false,
            progress: None,
            confirm: None,
            error: None,
            notice: None,
            logs: VecDeque::new(),
            log_scroll: 0,
            focus: Focus::Model(0),
            hit: Default::default(),
            log_rect: Default::default(),
        }
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        if self.logs.len() == LOG_CAP {
            self.logs.pop_front();
        }
        self.logs.push_back(line.into());
    }

    pub fn on_session_event(&mut self, ev: Event) {
        match ev {
            Event::WaitingForDevice { hints } => {
                if self.screen == Screen::Waiting {
                    self.hints = hints;
                }
            }
            Event::DeviceConnected { identity } => {
                if identity.model != self.selected_model {
                    self.notice = Some(format!(
                        "на шине {} ({}) — переключаюсь",
                        identity.model.label(),
                        identity.key
                    ));
                }
                self.selected_model = identity.model;
                self.identity = Some(identity);
                self.screen = Screen::Main;
                self.focus = Focus::TakeBackup;
                self.done = false;
                self.progress = None;
                // NOTE: `busy` is NOT cleared — a command queued while
                // waiting is already running on the worker
            }
            Event::BackupState(state) => {
                use jb_core::scenario::BackupState;
                self.backup = match state {
                    BackupState::None => BackupSnapshot::None,
                    BackupState::Verified { dir } => BackupSnapshot::Verified { dir },
                    BackupState::Unusable { dir, why } => BackupSnapshot::Unusable { dir, why },
                };
            }
            Event::Progress(ev) => self.on_progress(ev),
            Event::ConfirmRequested { plan, reply } => {
                self.confirm = Some(Confirm::Write {
                    plan,
                    reply: Some(reply),
                });
                self.focus = Focus::ModalOk;
            }
            Event::Finished(result) => {
                let finished = self.last_busy_label();
                self.busy = Busy::None;
                self.progress = None;
                match result {
                    Ok(()) => match finished {
                        // the column rebooted: flashing again needs a
                        // fresh connect, the session is stale
                        "jailbreak" | "restore" => self.done = true,
                        _ => {}
                    },
                    Err(e) => self.error = Some(format!("{e:#}")),
                }
            }
        }
    }

    /// Label of the op that just finished (called before `busy` is
    /// cleared — keep both in sync if Busy grows).
    fn last_busy_label(&self) -> &'static str {
        match self.busy {
            Busy::Jailbreak => "jailbreak",
            Busy::Backup => "backup",
            Busy::Restore => "restore",
            Busy::None => "",
        }
    }

    fn on_progress(&mut self, ev: Progress) {
        match ev {
            Progress::Step(msg) => {
                self.progress = None;
                self.push_log(msg);
            }
            Progress::PartitionStart { name, sectors } => {
                self.progress = Some(ActiveProgress {
                    label: name,
                    done: 0,
                    total: sectors,
                });
            }
            Progress::PartitionProgress { name, done, total } => {
                self.progress = Some(ActiveProgress {
                    label: name,
                    done,
                    total,
                });
            }
            Progress::PartitionDone { .. } => self.progress = None,
            Progress::TransferStart { name, bytes } => {
                self.progress = Some(ActiveProgress {
                    label: name,
                    done: 0,
                    total: bytes,
                });
            }
            Progress::TransferProgress { name, done, total } => {
                self.progress = Some(ActiveProgress {
                    label: name,
                    done,
                    total,
                });
            }
            Progress::TransferDone { .. } => self.progress = None,
        }
    }

    pub fn on_key(&mut self, key: ratatui::crossterm::event::KeyEvent) {
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
            _ => {}
        }
    }

    pub fn on_mouse(&mut self, mouse: ratatui::crossterm::event::MouseEvent) {
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
            Screen::Waiting => {
                self.screen = Screen::ModelSelect;
                self.focus = Focus::Model(0);
            }
            Screen::Main => self.request_quit(),
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

    fn activate(&mut self, focus: Focus) {
        match focus {
            Focus::Model(i) if self.screen == Screen::ModelSelect => {
                self.selected_model = if i == 0 {
                    Model::Capsule
                } else {
                    Model::CapsuleMini
                };
            }
            Focus::ModelNext => {
                self.screen = Screen::Waiting;
                self.hints = vec![crate::hint_text(self.selected_model).to_string()];
                self.focus = Focus::ModelNext;
            }
            Focus::SetAdbd => self.settings.adbd = !self.settings.adbd,
            Focus::SetFormat => self.settings.format_userdata = !self.settings.format_userdata,
            Focus::SetNoBackup => self.settings.no_backup = !self.settings.no_backup,
            Focus::TakeBackup => {
                if self.busy == Busy::None && !self.done {
                    self.busy = Busy::Backup;
                    self.notice = None;
                    self.error = None;
                    self.session.command(Command::Backup);
                }
            }
            Focus::Restore => {
                if self.busy == Busy::None && !self.done && self.backup.dir().is_some() {
                    self.busy = Busy::Restore;
                    self.notice = None;
                    self.error = None;
                    self.session.command(Command::Restore);
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
                    self.session
                        .command(Command::Jailbreak(self.settings.params()));
                }
            }
            Focus::ModalOk => match self.confirm.take() {
                Some(Confirm::Write { reply, .. }) => {
                    if let Some(reply) = reply {
                        let _ = reply.send(Decision::Proceed);
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

    /// Answer the write confirm with Abort (or just drop a local
    /// modal).
    fn cancel_confirm(&mut self) {
        if let Some(Confirm::Write { reply, .. }) = self.confirm.take()
            && let Some(reply) = reply
        {
            let _ = reply.send(Decision::Abort);
        }
        if self.screen == Screen::Main {
            self.focus = Focus::Flash;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jb_core::backend::Backend;
    use jb_core::scenario::Prepared;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Duration;

    /// The same fake as jb-core's session tests, reused through a
    /// fresh Session per case (the worker serves one op at a time).
    struct FakeBackend {
        identity: Identity,
    }

    impl jb_core::scenario::Platform for FakeBackend {
        fn ensure_backup(
            &mut self,
            no_backup: bool,
            _p: jb_core::progress::ProgressFn,
        ) -> anyhow::Result<Prepared> {
            Ok(Prepared {
                base: if no_backup { None } else { Some("x".into()) },
                dumped: false,
                dir: None,
                complete: true,
            })
        }
        fn backup_state(&self) -> jb_core::scenario::BackupState {
            jb_core::scenario::BackupState::None
        }
        fn data_size(&mut self, _p: jb_core::progress::ProgressFn) -> anyhow::Result<u64> {
            Ok(4096)
        }
        fn data_partition(&self) -> &'static str {
            "testpart"
        }
        fn describe_write(&self, _f: bool, _n: bool) -> String {
            "test".into()
        }
        fn describe_restore(&self) -> String {
            "test-restore".into()
        }
        fn build_data_image(
            &self,
            _b: Option<&Path>,
            _o: &Path,
            _s: u64,
            _f: &[jb_core::overlay::OverlayFile],
            _p: jb_core::progress::ProgressFn,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn write_data_image(
            &mut self,
            _i: &Path,
            _p: jb_core::progress::ProgressFn,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn restore_set(
            &mut self,
        ) -> anyhow::Result<(Vec<jb_core::scenario::RestoreItem>, Vec<String>)> {
            Ok((vec![], vec![]))
        }
        fn write_restore(
            &mut self,
            _items: &[jb_core::scenario::RestoreItem],
            _p: jb_core::progress::ProgressFn,
        ) -> anyhow::Result<()> {
            Ok(())
        }
        fn finish(&mut self, _p: jb_core::progress::ProgressFn) -> anyhow::Result<()> {
            Ok(())
        }
        fn report_backup(&self, _p: &Prepared) {}
    }

    impl Backend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn present(&self) -> anyhow::Result<bool> {
            Ok(true)
        }
        fn connect(
            &mut self,
            _w: Option<Duration>,
            _p: jb_core::progress::ProgressFn,
        ) -> anyhow::Result<Identity> {
            Ok(self.identity.clone())
        }
        fn platform(&mut self) -> &mut dyn jb_core::scenario::Platform {
            self
        }
    }

    fn app() -> App {
        App::new(Session::spawn(
            vec![Box::new(FakeBackend {
                identity: Identity {
                    model: Model::Capsule,
                    key: "KEY".into(),
                    firstgen: false,
                    caveats: vec![],
                },
            })],
            None,
        ))
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Drains worker events into the app until the predicate holds.
    fn pump(app: &mut App, until: impl Fn(&App) -> bool) {
        for _ in 0..100 {
            if until(app) {
                return;
            }
            match app.session.events().recv_timeout(Duration::from_secs(2)) {
                Ok(ev) => app.on_session_event(ev),
                Err(_) => panic!("событие не пришло"),
            }
        }
        panic!("условие не наступило");
    }

    #[test]
    fn screens_advance_and_model_switches_on_mismatch() {
        let mut a = app();
        assert_eq!(a.screen, Screen::ModelSelect);
        a.on_key(key(KeyCode::Tab)); // Model(1)
        a.on_key(key(KeyCode::Enter)); // select Mini
        assert_eq!(a.selected_model, Model::CapsuleMini);
        a.on_key(key(KeyCode::Tab)); // ModelNext
        a.on_key(key(KeyCode::Enter)); // -> Waiting
        assert_eq!(a.screen, Screen::Waiting);
        pump(&mut a, |a| a.screen == Screen::Main);
        // a Capsule connected while Mini was selected: switched
        assert_eq!(a.identity.as_ref().unwrap().model, Model::Capsule);
        assert_eq!(a.selected_model, Model::Capsule);
    }

    #[test]
    fn flash_sends_jailbreak_and_confirm_modal_proceeds() {
        let mut a = app();
        pump(&mut a, |a| a.screen == Screen::Main);
        // focus starts on TakeBackup; walk to Flash
        for _ in 0..6 {
            a.on_key(key(KeyCode::Tab));
        }
        assert_eq!(a.focus, Focus::Flash);
        a.settings.adbd = false;
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.busy, Busy::Jailbreak);
        pump(&mut a, |a| a.confirm.is_some());
        let Confirm::Write { plan, .. } = a.confirm.as_ref().unwrap() else {
            panic!("не write-модалка")
        };
        assert_eq!(plan.partition, "testpart");
        assert!(!plan.fresh || plan.mode == "test"); // just shape smoke
        a.on_key(key(KeyCode::Enter)); // ModalOk -> Proceed
        assert!(a.confirm.is_none());
        pump(&mut a, |a| a.busy == Busy::None);
        assert!(a.error.is_none());
        assert!(a.done); // jailbreak finished -> rebooted state
    }

    #[test]
    fn confirm_abort_reaches_the_worker() {
        let mut a = app();
        pump(&mut a, |a| a.screen == Screen::Main);
        a.activate(Focus::Flash);
        pump(&mut a, |a| a.confirm.is_some());
        a.on_key(key(KeyCode::Esc)); // cancel -> Abort
        assert!(a.confirm.is_none());
        pump(&mut a, |a| a.busy == Busy::None);
        assert!(a.error.is_some());
    }

    #[test]
    fn quit_is_blocked_while_busy() {
        let mut a = app();
        pump(&mut a, |a| a.screen == Screen::Main);
        a.activate(Focus::Flash);
        assert_ne!(a.busy, Busy::None);
        a.on_key(key(KeyCode::Char('q')));
        assert!(!a.quit);
        assert!(a.notice.is_some());
    }

    #[test]
    fn settings_toggle_and_params() {
        let mut s = Settings::default();
        assert!(s.params().adbd);
        s.adbd = false;
        s.format_userdata = true;
        let p = s.params();
        assert!(!p.adbd && p.format_userdata && !p.no_backup);
    }

    #[test]
    fn progress_events_map_to_the_line_and_log() {
        let mut a = app();
        a.on_session_event(Event::Progress(Progress::PartitionStart {
            name: "userdata".into(),
            sectors: 100,
        }));
        assert_eq!(a.progress.as_ref().unwrap().label, "userdata");
        a.on_session_event(Event::Progress(Progress::PartitionProgress {
            name: "userdata".into(),
            done: 40,
            total: 100,
        }));
        assert_eq!(a.progress.as_ref().unwrap().done, 40);
        a.on_session_event(Event::Progress(Progress::Step("шаг".into())));
        assert!(a.progress.is_none());
        assert_eq!(a.logs.back().unwrap(), "шаг");
    }

    #[test]
    fn focus_ring_cycles_and_modal_captures() {
        let mut a = app();
        assert_eq!(a.focus, Focus::Model(0));
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.focus, Focus::Model(1));
        a.on_key(key(KeyCode::BackTab));
        assert_eq!(a.focus, Focus::Model(0));
        // a modal captures the ring
        a.confirm = Some(Confirm::DeleteBackup {
            dir: "/tmp/x".into(),
        });
        a.on_key(key(KeyCode::Tab));
        assert_eq!(a.focus, Focus::ModalCancel);
    }
}
