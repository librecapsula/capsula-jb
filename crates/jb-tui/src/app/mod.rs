//! The TUI application state: the screen machine (model select →
//! waiting → main) and the session-event handling. The session worker
//! starts only when the model is committed — it polls exactly that
//! model's backend. Plain data + methods, testable without a
//! terminal; the input routing lives in [`input`], the tests in
//! [`tests`].

pub mod input;
#[cfg(test)]
mod tests;

pub use input::Focus;

use jb_core::backend::Backend;
use jb_core::progress::Progress;
use jb_core::scenario::WritePlan;
use jb_core::session::{Event, Identity, Model, Session};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
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
    fn params(&self) -> jb_core::scenario::Params {
        jb_core::scenario::Params {
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
    Verified { dir: PathBuf },
    Unusable { dir: PathBuf, why: Vec<String> },
}

impl BackupSnapshot {
    pub fn dir(&self) -> Option<&Path> {
        match self {
            BackupSnapshot::Verified { dir } | BackupSnapshot::Unusable { dir, .. } => Some(dir),
            _ => None,
        }
    }
}

/// The active progress line.
#[derive(Debug, Clone)]
pub struct ActiveProgress {
    pub label: String,
    pub done: u64,
    pub total: u64,
}

#[derive(Debug, Clone)]
pub enum Confirm {
    /// The scenario asks to allow the write (the worker is BLOCKED
    /// until the reply).
    Write {
        plan: WritePlan,
        reply: Option<Sender<jb_core::session::Decision>>,
    },
    /// Local: delete the backup directory.
    DeleteBackup { dir: PathBuf },
}

pub struct App {
    /// The backend factory of the binary (jb-cli's platform dispatch).
    pub for_model: fn(Model) -> Box<dyn Backend>,
    /// The session worker, spawned at the model commit — `None`
    /// while the model menu is up.
    pub session: Option<Session>,
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
    pub fn new(for_model: fn(Model) -> Box<dyn Backend>) -> Self {
        App {
            for_model,
            session: None,
            screen: Screen::ModelSelect,
            quit: false,
            selected_model: Model::Capsula,
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

    /// The live session; commands are only reachable past the model
    /// menu, where the worker already exists.
    pub(crate) fn session(&self) -> &Session {
        self.session
            .as_ref()
            .expect("команда без запущенной сессии")
    }

    /// Drain the pending worker events of this tick into the state
    /// machine.
    pub fn poll_events(&mut self) {
        loop {
            let ev = match self.session.as_ref() {
                Some(s) => s.events().try_recv(),
                None => return,
            };
            match ev {
                Ok(ev) => self.on_session_event(ev),
                Err(_) => return,
            }
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
                // The worker polls only the chosen model's backend, so
                // the identity is that model by construction — no
                // switching.
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
}
