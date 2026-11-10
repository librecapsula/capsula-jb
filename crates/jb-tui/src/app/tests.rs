//! State-machine tests over a fake backend — no terminal, no hardware.

use super::*;
use jb_core::backend::Backend;
use jb_core::progress::Progress;
use jb_core::scenario::Prepared;
use jb_core::session::{Event, Identity, Model, Session};
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
                model: Model::Capsula,
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
    assert_eq!(a.selected_model, Model::CapsulaMiniAmlogic);
    a.on_key(key(KeyCode::Tab)); // ModelNext
    a.on_key(key(KeyCode::Enter)); // -> Waiting
    assert_eq!(a.screen, Screen::Waiting);
    pump(&mut a, |a| a.screen == Screen::Main);
    // a Capsule connected while Mini was selected: switched
    assert_eq!(a.identity.as_ref().unwrap().model, Model::Capsula);
    assert_eq!(a.selected_model, Model::Capsula);
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
fn ctrl_c_quits_when_idle_and_is_gated_while_busy() {
    let mut a = app();
    a.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(a.quit);

    let mut a = app();
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
