//! capsula-jb — the GPUI GUI.
//!
//! The GUI opens when the tool is launched without a single argument
//! (the branch in jb-cli/src/main.rs) on macOS and Windows; on Linux
//! the no-args run keeps the CLI scenario (gpui needs system
//! X11/Wayland libraries there, which the aarch64 CI cross-build does
//! not have). For now the window is a single welcome screen saying
//! that we are waiting for the speaker; device detection is the next
//! iteration — a background bus poll feeding updates into the view.

use gpui::{
    App, Application, Bounds, Context, FocusHandle, KeyBinding, SharedString, TitlebarOptions,
    Window, WindowBounds, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

actions!(welcome, [CloseWindow]);

/// Launch the GUI; returns when the last window is closed.
pub fn run() -> anyhow::Result<()> {
    Application::new().run(|cx: &mut App| {
        // macOS keeps the process alive after the last window closes
        // (document-app behavior: the dock icon waits for a new
        // window). The tool IS its window — quit when it is gone.
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        // Without an app menu there is no system Close either; bind
        // the window-close action (cmd-w / ctrl-w) like the gpui
        // on_window_close_quit example does.
        #[cfg(target_os = "macos")]
        let  close_key = "cmd-w";
        #[cfg(not(target_os = "macos"))]
        let close_key = "ctrl-w";
        cx.bind_keys([KeyBinding::new(close_key, CloseWindow, None)]);

        let bounds = Bounds::centered(None, size(px(640.), px(420.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Capsula JB")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    let screen = WaitingScreen::new(cx);
                    screen.focus_handle.focus(window);
                    screen
                })
            },
        )
        .expect("не удалось открыть окно");
        cx.activate(true);
    });
    Ok(())
}

/// Root view of the welcome window: a single centered message that
/// the tool is waiting for the speaker on USB. The texts live in the
/// state so the upcoming device-poll step can flip them without
/// reworking the layout.
struct WaitingScreen {
    focus_handle: FocusHandle,
    title: SharedString,
    message: SharedString,
    hint: SharedString,
}

impl WaitingScreen {
    fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            title: "Capsula JB".into(),
            message: "Ждём подключения колонки…".into(),
            hint: "Подключите колонку к компьютеру по USB".into(),
        }
    }
}

impl Render for WaitingScreen {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .on_action(|_: &CloseWindow, window, _| window.remove_window())
            // The special ".SystemUIFont" name resolves to the platform
            // system font — no font assets to load.
            .font_family(".SystemUIFont")
            .flex()
            .flex_col()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .bg(rgb(0x1e1e1e))
            .text_color(rgb(0xe6e6e6))
            .child(div().text_3xl().child(self.title.clone()))
            .child(div().text_xl().child(self.message.clone()))
            .child(
                div()
                    .text_base()
                    .text_color(rgb(0x9a9a9a))
                    .child(self.hint.clone()),
            )
    }
}
