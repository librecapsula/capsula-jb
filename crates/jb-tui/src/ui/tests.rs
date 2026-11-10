//! Render tests over ratatui's TestBackend — the hit rects of the
//! focusable controls, mouse clicks and the log wheel.

use super::render;
use crate::app::{App, Busy, Focus, Screen};
use jb_core::session::{Identity, Model, Session};

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
        ) -> anyhow::Result<Identity> {
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

fn main_screen_app() -> App {
    let mut a = app();
    a.screen = Screen::Main;
    a.identity = Some(Identity {
        model: Model::Capsula,
        key: "SN".into(),
        firstgen: false,
        caveats: vec![],
    });
    a
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
    let mut a = main_screen_app();
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
    let mut a = main_screen_app();
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
    let mut a = main_screen_app();
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
