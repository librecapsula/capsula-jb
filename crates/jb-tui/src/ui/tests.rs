//! Mouse behavior over ratatui's TestBackend — clicks focus and
//! activate, the wheel scrolls the log. The drawing itself is not
//! verified.

use super::render;
use crate::app::{App, Busy, Confirm, Focus, Screen};
use jb_core::backend::{Backend, ConnectError};
use jb_core::model::Model;
use jb_core::session::Identity;

/// A backend that is never present: the session worker of the tests
/// polls quietly and never connects (the click tests drive the state
/// machine by hand).
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
    ) -> Result<jb_core::session::Identity, ConnectError> {
        unreachable!()
    }
    fn platform(&mut self) -> &mut dyn jb_core::scenario::Platform {
        unreachable!()
    }
}

fn noop_for_model(_: Model) -> Box<dyn Backend> {
    Box::new(NoBackend)
}

fn app() -> App {
    App::new(noop_for_model)
}

fn main_screen_app() -> App {
    let mut a = app();
    a.activate(Focus::Model(0)); // spawn the idle worker
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

/// The preparing screen is control-less: it renders (the log pane
/// included) and registers no hit rects — nothing to click while the
/// worker brings the device up.
#[test]
fn preparing_screen_renders_without_controls() {
    let mut a = app();
    a.activate(Focus::Model(0));
    a.screen = Screen::Preparing;
    a.push_log("лоадер (330d): сбрасываю в MaskROM");
    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut a)).unwrap();
    assert!(a.hit.is_empty());
}

/// The first-gen modal is Continue-only: the OK button covers the
/// whole button row and no cancel button is registered.
#[test]
fn firstgen_modal_is_continue_only() {
    let mut a = app();
    a.activate(Focus::Model(0));
    a.screen = Screen::Waiting;
    a.confirm = Some(Confirm::Firstgen {
        notice: vec!["кажется, первое поколение".into()],
        reply: None,
    });
    a.focus = Focus::ModalOk;
    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut a)).unwrap();
    assert!(a.hit.contains_key(&Focus::ModalOk));
    assert!(!a.hit.contains_key(&Focus::ModalCancel));
}

/// The connect-error modal is Continue-only as well (Esc aborts the
/// run instead of a cancel button).
#[test]
fn reconnect_modal_is_continue_only() {
    let mut a = app();
    a.activate(Focus::Model(0));
    a.screen = Screen::Waiting;
    a.confirm = Some(Confirm::Reconnect {
        why: "wrong device mode: лоадер (330d) не отвечает".into(),
        reply: None,
    });
    a.focus = Focus::ModalOk;
    let backend = ratatui::backend::TestBackend::new(100, 30);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal.draw(|f| render(f, &mut a)).unwrap();
    assert!(a.hit.contains_key(&Focus::ModalOk));
    assert!(!a.hit.contains_key(&Focus::ModalCancel));
}
