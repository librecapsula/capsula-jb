//! The event-driven session: a worker thread that owns the backends
//! and drives the blocking scenario, plus the command/event channels
//! every UI (the CLI loop, the TUI) talks over. Logs keep flowing
//! through the `log` facade — each UI installs its own sink.
//!
//! The worker absorbs the old `pick_backend` loop: it waits for a
//! device of any known platform, connects it (identity event), reports
//! the local backup state and then serves [`Command`]s one at a time.
//! A confirmation requested mid-operation blocks the worker until the
//! UI answers (or its end of the channel closes — that aborts).

use crate::backend::Backend;
use crate::progress::Progress;
use crate::scenario::{self, BackupState, Params, WritePlan};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

/// The bus poll cadence while waiting for a device (the amlogic
/// download window is ~450 ms; the rockchip monitor polls at 50 ms).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The supported speaker models (the TUI's first screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Model {
    /// The big Capsula (RK3308, rockchip).
    Capsule,
    /// The late-revision Capsula Mini (A113X, amlogic).
    CapsuleMini,
}

impl Model {
    /// The human name shown in UIs.
    pub fn label(&self) -> &'static str {
        match self {
            Model::Capsule => "Капсула",
            Model::CapsuleMini => "Капсула Мини",
        }
    }

    /// The backup layout segment for the model: backups live in
    /// `backup/<dir_name>/<key>/` — identity keys of different
    /// platforms may collide, so the layout is per-model.
    pub fn dir_name(&self) -> &'static str {
        match self {
            Model::Capsule => "capsule",
            Model::CapsuleMini => "capsule-mini",
        }
    }
}

/// The connected column, as [`Backend::connect`] identifies it.
#[derive(Debug, Clone)]
pub struct Identity {
    pub model: Model,
    /// The backup key: the loader serial (rockchip) or the deviceid
    /// from the u-boot environment (amlogic).
    pub key: String,
    /// First generation (rockchip only): the full backup is impossible
    /// (0xCC above the 32 MiB read mask), userdata is always built
    /// from scratch.
    pub firstgen: bool,
    /// Platform caveats to show next to the identity (may be empty).
    pub caveats: Vec<String>,
}

/// What the UI asks the session to run. Served one at a time; the
/// result arrives as [`Event::Finished`].
#[derive(Debug, Clone)]
pub enum Command {
    Jailbreak(Params),
    Backup,
    Restore,
}

/// The UI's answer to [`Event::ConfirmRequested`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Proceed,
    Abort,
}

/// Everything a UI needs to render the run.
#[derive(Debug)]
pub enum Event {
    /// The worker is waiting for a device; `hints` are the
    /// platform-specific "how to enter the download mode" texts.
    WaitingForDevice {
        hints: Vec<String>,
    },
    /// A device connected and identified.
    DeviceConnected {
        identity: Identity,
    },
    /// The connected column's backup situation (also re-emitted after
    /// every finished operation — a backup may have appeared).
    BackupState(BackupState),
    Progress(Progress),
    /// The scenario is about to write and needs a decision. The worker
    /// is BLOCKED until `reply` anwers; dropping the sender aborts.
    ConfirmRequested {
        plan: WritePlan,
        reply: Sender<Decision>,
    },
    /// The operation ended (Ok = the scenario completed). The worker
    /// stays alive for the next command.
    Finished(Result<()>),
}

/// The UI-side handle: send commands, receive events. Dropping it ends
/// the worker.
pub struct Session {
    cmd: Sender<Command>,
    events: Receiver<Event>,
}

impl Session {
    /// Start the worker over the given backends. Commands may be sent
    /// immediately — they queue until a device connects.
    pub fn spawn(backends: Vec<Box<dyn Backend>>, wait: Option<Duration>) -> Self {
        let (ev_tx, ev_rx) = channel::<Event>();
        let (cmd_tx, cmd_rx) = channel::<Command>();
        std::thread::Builder::new()
            .name("jb-session".into())
            .spawn(move || worker(backends, wait, cmd_rx, ev_tx))
            .expect("не удалось запустить поток сессии");
        Session {
            cmd: cmd_tx,
            events: ev_rx,
        }
    }

    /// Queue a command (served after a device is connected, one at a
    /// time). Ignored if the worker is gone.
    pub fn command(&self, cmd: Command) {
        let _ = self.cmd.send(cmd);
    }

    /// The event stream. `recv() == Err` means the worker died
    /// unexpectedly (a panic).
    pub fn events(&self) -> &Receiver<Event> {
        &self.events
    }
}

/// The worker: wait for a device → connect → report identity and
/// backup state → serve commands until the handle drops.
fn worker(
    backends: Vec<Box<dyn Backend>>,
    wait: Option<Duration>,
    commands: Receiver<Command>,
    events: Sender<Event>,
) {
    let mut backend = match wait_for_backend(backends, wait, &events) {
        Ok(b) => b,
        Err(e) => {
            let _ = events.send(Event::Finished(Err(e)));
            return;
        }
    };
    let progress = |ev: Progress| {
        let _ = events.send(Event::Progress(ev));
    };
    let confirm = |plan: &WritePlan| -> Result<()> {
        let (reply, answer) = channel::<Decision>();
        if events
            .send(Event::ConfirmRequested {
                plan: plan.clone(),
                reply,
            })
            .is_err()
        {
            bail!("прервано пользователем");
        }
        match answer.recv() {
            Ok(Decision::Proceed) => Ok(()),
            Ok(Decision::Abort) | Err(_) => bail!("прервано пользователем"),
        }
    };

    for cmd in commands {
        let result = match cmd {
            Command::Jailbreak(params) => {
                scenario::jailbreak(backend.platform(), &params, &confirm, &progress)
            }
            Command::Backup => scenario::backup(backend.platform(), &progress),
            Command::Restore => scenario::restore(backend.platform(), &confirm, &progress),
        };
        let _ = events.send(Event::Finished(result));
        let _ = events.send(Event::BackupState(backend.platform().backup_state()));
    }
}

/// The old CLI `pick_backend` loop: poll every backend's presence,
/// connect the first one that shows up. Emits [`Event::WaitingForDevice`]
/// once while the bus is empty.
fn wait_for_backend(
    mut backends: Vec<Box<dyn Backend>>,
    wait: Option<Duration>,
    events: &Sender<Event>,
) -> Result<Box<dyn Backend>> {
    let start = Instant::now();
    let mut hint_shown = false;
    loop {
        let mut chosen: Option<usize> = None;
        for (i, b) in backends.iter().enumerate() {
            if b.present()? {
                chosen = Some(i);
                break;
            }
        }
        if let Some(i) = chosen {
            let mut b = backends.swap_remove(i);
            log::info!("платформа: {}", b.name());
            let progress = |ev: Progress| {
                let _ = events.send(Event::Progress(ev));
            };
            let identity = b.connect(wait, &progress)?;
            let _ = events.send(Event::DeviceConnected {
                identity: identity.clone(),
            });
            let _ = events.send(Event::BackupState(b.platform().backup_state()));
            return Ok(b);
        }
        if !hint_shown {
            let hints = backends
                .iter()
                .filter_map(|b| b.wait_hint().map(str::to_owned))
                .collect::<Vec<_>>();
            let _ = events.send(Event::WaitingForDevice { hints });
            hint_shown = true;
        }
        if let Some(t) = wait
            && start.elapsed() >= t
        {
            bail!("ни одна поддерживаемая платформа не появилась на шине (--wait)");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::Prepared;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};

    /// A scriptable backend: presence is a counter (0 = absent), the
    /// platform records hook calls and echoes a canned identity.
    struct FakeBackend {
        present_after: u32,
        polls: RefCell<u32>,
        calls: RefCell<Vec<&'static str>>,
        identity: Identity,
        /// What ensure_backup returns as the base.
        base: Option<PathBuf>,
    }

    impl FakeBackend {
        fn online() -> Self {
            FakeBackend {
                present_after: 1,
                polls: RefCell::new(0),
                calls: RefCell::new(vec![]),
                identity: Identity {
                    model: Model::Capsule,
                    key: "TESTSERIAL".into(),
                    firstgen: false,
                    caveats: vec![],
                },
                base: None,
            }
        }
    }

    impl crate::scenario::Platform for FakeBackend {
        fn ensure_backup(
            &mut self,
            no_backup: bool,
            _progress: crate::progress::ProgressFn,
        ) -> Result<Prepared> {
            self.calls.borrow_mut().push("ensure_backup");
            let base = if no_backup { None } else { self.base.clone() };
            Ok(Prepared {
                base,
                dumped: false,
                dir: None,
                complete: true,
            })
        }

        fn backup_state(&self) -> BackupState {
            BackupState::None
        }

        fn data_size(&mut self, _progress: crate::progress::ProgressFn) -> Result<u64> {
            self.calls.borrow_mut().push("data_size");
            Ok(4096)
        }

        fn data_partition(&self) -> &'static str {
            "testpart"
        }

        fn describe_write(&self, _fresh: bool, _no_backup: bool) -> String {
            "test write".into()
        }

        fn describe_restore(&self) -> String {
            "test restore".into()
        }

        fn build_data_image(
            &self,
            _base: Option<&Path>,
            _out: &Path,
            _size: u64,
            _files: &[crate::overlay::OverlayFile],
            _progress: crate::progress::ProgressFn,
        ) -> Result<()> {
            self.calls.borrow_mut().push("build");
            Ok(())
        }

        fn write_data_image(
            &mut self,
            _image: &Path,
            _progress: crate::progress::ProgressFn,
        ) -> Result<()> {
            self.calls.borrow_mut().push("write");
            Ok(())
        }

        fn restore_set(&mut self) -> Result<(Vec<crate::scenario::RestoreItem>, Vec<String>)> {
            Ok((vec![], vec![]))
        }

        fn write_restore(
            &mut self,
            _items: &[crate::scenario::RestoreItem],
            _progress: crate::progress::ProgressFn,
        ) -> Result<()> {
            Ok(())
        }

        fn finish(&mut self, _progress: crate::progress::ProgressFn) -> Result<()> {
            self.calls.borrow_mut().push("finish");
            Ok(())
        }

        fn report_backup(&self, _prepared: &Prepared) {}
    }

    impl Backend for FakeBackend {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn present(&self) -> Result<bool> {
            *self.polls.borrow_mut() += 1;
            Ok(*self.polls.borrow() >= self.present_after)
        }

        fn connect(
            &mut self,
            _wait: Option<Duration>,
            _progress: crate::progress::ProgressFn,
        ) -> Result<Identity> {
            self.calls.borrow_mut().push("connect");
            Ok(self.identity.clone())
        }

        fn platform(&mut self) -> &mut dyn crate::scenario::Platform {
            self
        }
    }

    #[test]
    fn connect_then_jailbreak_event_sequence() {
        let session = Session::spawn(vec![Box::new(FakeBackend::online())], None);
        session.command(Command::Jailbreak(Params::default()));

        let mut saw_identity = false;
        let mut saw_backup_state = false;
        loop {
            match session.events().recv().unwrap() {
                Event::WaitingForDevice { .. } => {}
                Event::DeviceConnected { identity } => {
                    assert_eq!(identity.key, "TESTSERIAL");
                    saw_identity = true;
                }
                Event::BackupState(_) => saw_backup_state = true,
                Event::ConfirmRequested { plan, reply } => {
                    assert_eq!(plan.mode, "test write");
                    let _ = reply.send(Decision::Proceed);
                }
                Event::Finished(r) => {
                    r.unwrap();
                    break;
                }
                Event::Progress(_) => {}
            }
        }
        assert!(saw_identity && saw_backup_state);
    }

    #[test]
    fn confirm_abort_stops_before_any_write() {
        let session = Session::spawn(vec![Box::new(FakeBackend::online())], None);
        session.command(Command::Jailbreak(Params::default()));

        loop {
            match session.events().recv().unwrap() {
                Event::ConfirmRequested { reply, .. } => {
                    let _ = reply.send(Decision::Abort);
                }
                Event::Finished(r) => {
                    let err = r.unwrap_err();
                    assert_eq!(err.to_string(), "прервано пользователем");
                    break;
                }
                _ => {}
            }
        }
    }

    #[test]
    fn worker_error_surfaces_as_finished() {
        // present() fails immediately -> wait_for_backend bails ->
        // Finished(Err) and the worker ends.
        struct Broken;
        impl Backend for Broken {
            fn name(&self) -> &'static str {
                "broken"
            }
            fn present(&self) -> Result<bool> {
                bail!("usb сломан")
            }
            fn connect(
                &mut self,
                _wait: Option<Duration>,
                _progress: crate::progress::ProgressFn,
            ) -> Result<Identity> {
                unreachable!()
            }
            fn platform(&mut self) -> &mut dyn crate::scenario::Platform {
                unreachable!()
            }
        }
        impl crate::scenario::Platform for Broken {
            fn ensure_backup(
                &mut self,
                _no_backup: bool,
                _progress: crate::progress::ProgressFn,
            ) -> Result<Prepared> {
                unreachable!()
            }
            fn backup_state(&self) -> BackupState {
                BackupState::None
            }
            fn data_size(&mut self, _p: crate::progress::ProgressFn) -> Result<u64> {
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
                _b: Option<&Path>,
                _o: &Path,
                _s: u64,
                _f: &[crate::overlay::OverlayFile],
                _p: crate::progress::ProgressFn,
            ) -> Result<()> {
                unreachable!()
            }
            fn write_data_image(
                &mut self,
                _i: &Path,
                _p: crate::progress::ProgressFn,
            ) -> Result<()> {
                unreachable!()
            }
            fn restore_set(&mut self) -> Result<(Vec<crate::scenario::RestoreItem>, Vec<String>)> {
                unreachable!()
            }
            fn write_restore(
                &mut self,
                _items: &[crate::scenario::RestoreItem],
                _p: crate::progress::ProgressFn,
            ) -> Result<()> {
                unreachable!()
            }
            fn finish(&mut self, _p: crate::progress::ProgressFn) -> Result<()> {
                unreachable!()
            }
            fn report_backup(&self, _p: &Prepared) {}
        }

        let session = Session::spawn(vec![Box::new(Broken)], None);
        match session.events().recv().unwrap() {
            Event::Finished(r) => assert!(r.is_err()),
            other => panic!("неожиданное событие: {other:?}"),
        }
    }
}
