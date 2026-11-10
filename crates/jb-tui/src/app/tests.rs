//! State-machine tests over a fake backend — no terminal, no hardware.

use super::*;
use jb_core::backend::{Backend, ConnectError};
use jb_core::model::Model;
use jb_core::progress::Progress;
use jb_core::scenario::Prepared;
use jb_core::session::{Event, Identity};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::time::Duration;

/// The same fake as jb-core's session tests, reused through a
/// fresh Session per case (the worker serves one op at a time).
/// `fatals`: how many first connects fail with a Fatal (a
/// device-state problem, the reconnect modal); `attentions`: how many
/// next connects return the degraded-flow Attention before the
/// identity succeeds (the rockchip first-gen settlement).
struct FakeBackend {
    identity: Identity,
    fatals: std::cell::Cell<u32>,
    attentions: std::cell::Cell<u32>,
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
    ) -> Result<Identity, ConnectError> {
        if self.fatals.get() > 0 {
            self.fatals.set(self.fatals.get() - 1);
            return Err(ConnectError::Fatal(anyhow::anyhow!(
                "wrong device mode: лоадер (330d) не отвечает"
            )));
        }
        if self.attentions.get() > 0 {
            self.attentions.set(self.attentions.get() - 1);
            return Err(ConnectError::Attention {
                notice: vec!["кажется, первое поколение".into()],
            });
        }
        Ok(self.identity.clone())
    }
    fn platform(&mut self) -> &mut dyn jb_core::scenario::Platform {
        self
    }
}

fn fake_identity() -> Identity {
    Identity {
        model: Model::Capsula,
        key: "KEY".into(),
        firstgen: false,
        caveats: vec![],
    }
}

fn fake_for_model(_: Model) -> Box<dyn Backend> {
    Box::new(FakeBackend {
        identity: fake_identity(),
        fatals: std::cell::Cell::new(0),
        attentions: std::cell::Cell::new(0),
    })
}

/// The fake that asks for the degraded-flow consent on the first
/// connect (the rockchip first-generation path).
fn firstgen_for_model(_: Model) -> Box<dyn Backend> {
    Box::new(FakeBackend {
        identity: fake_identity(),
        fatals: std::cell::Cell::new(0),
        attentions: std::cell::Cell::new(1),
    })
}

/// The fake whose first connect fails with a Fatal (a wedged loader
/// persona — the reconnect modal path).
fn failing_for_model(_: Model) -> Box<dyn Backend> {
    Box::new(FakeBackend {
        identity: fake_identity(),
        fatals: std::cell::Cell::new(1),
        attentions: std::cell::Cell::new(0),
    })
}

fn app() -> App {
    App::new(fake_for_model)
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
        let ev = match app
            .session
            .as_ref()
            .expect("сессия не запущена")
            .events()
            .recv_timeout(Duration::from_secs(2))
        {
            Ok(ev) => ev,
            Err(_) => panic!("событие не пришло"),
        };
        app.on_session_event(ev);
    }
    panic!("условие не наступило");
}

#[test]
fn worker_spawns_at_the_model_commit_and_advances() {
    let mut a = app();
    assert!(a.session.is_none()); // no worker behind the menu
    a.activate(Focus::Model(0)); // commit -> spawn + Waiting
    assert!(a.session.is_some());
    assert_eq!(a.screen, Screen::Waiting);
    // device seen -> Connecting -> the preparing screen -> Main
    pump(&mut a, |a| a.screen == Screen::Preparing);
    pump(&mut a, |a| a.screen == Screen::Main);
    assert_eq!(a.identity.as_ref().unwrap().key, "KEY");
}

/// The rockchip first-generation settlement: the connect asks for the
/// degraded-flow consent — the modal over the waiting screen; Continue
/// (ModalOk) answers Proceed, the worker re-connects and lands on Main.
#[test]
fn firstgen_attention_modal_continues_to_main() {
    let mut a = App::new(firstgen_for_model);
    a.activate(Focus::Model(0));
    pump(&mut a, |a| {
        matches!(a.confirm, Some(Confirm::Firstgen { .. }))
    });
    // the modal sits over the waiting screen (the loader-mode hints)
    assert_eq!(a.screen, Screen::Waiting);
    assert_eq!(a.focus, Focus::ModalOk);
    a.on_key(key(KeyCode::Enter)); // ModalOk -> Proceed
    assert!(a.confirm.is_none());
    pump(&mut a, |a| a.screen == Screen::Main);
    assert_eq!(a.identity.as_ref().unwrap().key, "KEY");
}

/// Esc on the first-gen modal aborts the run: the worker dies with
/// "прервано пользователем" and the error surfaces.
#[test]
fn firstgen_attention_esc_aborts() {
    let mut a = App::new(firstgen_for_model);
    a.activate(Focus::Model(0));
    pump(&mut a, |a| {
        matches!(a.confirm, Some(Confirm::Firstgen { .. }))
    });
    a.on_key(key(KeyCode::Esc));
    assert!(a.confirm.is_none());
    pump(&mut a, |a| a.error.is_some());
    assert!(a.error.as_deref().unwrap().contains("прервано"));
}

/// A Fatal connect error (a wedged loader persona) becomes the
/// reconnect modal over the waiting screen; Continue answers Proceed,
/// the worker waits for the column and lands on Main.
#[test]
fn connect_fatal_modal_continues_to_main() {
    let mut a = App::new(failing_for_model);
    a.activate(Focus::Model(0));
    pump(&mut a, |a| {
        matches!(a.confirm, Some(Confirm::Reconnect { .. }))
    });
    // the modal sits over the waiting screen (the loader-mode hints)
    assert_eq!(a.screen, Screen::Waiting);
    assert_eq!(a.focus, Focus::ModalOk);
    let Some(Confirm::Reconnect { why, .. }) = &a.confirm else {
        unreachable!();
    };
    assert!(why.contains("wrong device mode"));
    a.on_key(key(KeyCode::Enter)); // ModalOk -> Proceed
    assert!(a.confirm.is_none());
    pump(&mut a, |a| a.screen == Screen::Main);
    assert_eq!(a.identity.as_ref().unwrap().key, "KEY");
}

/// Esc on the reconnect modal aborts the run like the first-gen one.
#[test]
fn connect_fatal_esc_aborts() {
    let mut a = App::new(failing_for_model);
    a.activate(Focus::Model(0));
    pump(&mut a, |a| {
        matches!(a.confirm, Some(Confirm::Reconnect { .. }))
    });
    a.on_key(key(KeyCode::Esc));
    assert!(a.confirm.is_none());
    pump(&mut a, |a| a.error.is_some());
    assert!(a.error.as_deref().unwrap().contains("прервано"));
}

#[test]
fn arrows_then_enter_choose_the_model_and_seed_the_hints() {
    let mut a = app();
    a.on_key(key(KeyCode::Down)); // cursor to the second row
    a.on_key(key(KeyCode::Enter)); // choose -> spawn + Waiting
    assert_eq!(a.selected_model, Model::CapsulaMiniAmlogic);
    assert_eq!(a.screen, Screen::Waiting);
    assert!(a.session.is_some());
    assert_eq!(a.hints, vec![crate::hint_text(Model::CapsulaMiniAmlogic)]);
}

#[test]
fn esc_on_waiting_quits_the_tool() {
    let mut a = app();
    a.activate(Focus::Model(0));
    a.on_key(key(KeyCode::Esc));
    assert!(a.quit);
}

#[test]
fn flash_sends_jailbreak_and_confirm_modal_proceeds() {
    let mut a = app();
    a.activate(Focus::Model(0)); // commit -> the worker starts
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
    a.activate(Focus::Model(0)); // commit -> the worker starts
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
    a.activate(Focus::Model(0)); // commit -> the worker starts
    pump(&mut a, |a| a.screen == Screen::Main);
    a.activate(Focus::Flash);
    assert_ne!(a.busy, Busy::None);
    a.on_key(key(KeyCode::Char('q')));
    assert!(!a.quit);
    assert!(a.notice.is_some());
}

#[test]
fn ctrl_c_quits_when_idle_and_is_gated_while_busy() {
    let mut a = app();
    a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(a.quit);

    let mut a = app();
    a.activate(Focus::Model(0)); // commit -> the worker starts
    pump(&mut a, |a| a.screen == Screen::Main);
    a.activate(Focus::Flash);
    a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
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
