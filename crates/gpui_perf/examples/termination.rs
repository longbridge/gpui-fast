//! Run on macOS, then press Cmd-Q twice. The first request cancels after 500 ms;
//! the second allows final shutdown. Heartbeats continue while preparation waits.
//! `--self-test` exercises the native runloop with a hidden window and exits on approval.
extern crate gpui_fast as gpui;
extern crate gpui_platform_fast as gpui_platform;

use gpui::{
    App, Bounds, Context, FocusHandle, KeyBinding, Menu, MenuItem, Window, WindowBounds,
    WindowOptions, actions, div, prelude::*, px, rgb, size,
};
use std::{cell::RefCell, rc::Rc, time::Duration};

actions!(termination, [Quit]);

struct Example {
    focus_handle: FocusHandle,
}

#[derive(Default)]
struct RunState {
    attempts: usize,
    completed: usize,
    heartbeats: usize,
    cleanup: usize,
}

fn quit(_: &Quit, cx: &mut App) {
    eprintln!("Quit action: cx.quit()");
    cx.quit();
}

impl Render for Example {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .key_context("Termination")
            .on_action(|action: &Quit, _, cx| quit(action, cx))
            .size_full()
            .flex()
            .flex_col()
            .justify_center()
            .items_center()
            .gap_4()
            .p_6()
            .bg(rgb(0x20242b))
            .font_family(".SystemUIFont")
            .text_color(rgb(0xffffff))
            .text_lg()
            .child("First quit cancels after 500 ms. Second quit exits.")
            .child("Heartbeats and preparation results appear in the terminal.")
            .child(
                div()
                    .id("quit")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x365f91))
                    .cursor_pointer()
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(Quit), cx))
                    .child("Quit (Enter / Cmd-Q)"),
            )
    }
}

fn main() {
    let self_test = std::env::args().skip(1).any(|arg| arg == "--self-test");
    gpui_platform::application().run(move |cx: &mut App| {
        cx.on_action(quit);
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("enter", Quit, Some("Termination")),
        ]);
        cx.set_menus([Menu::new("Termination").items([MenuItem::action("Quit", Quit)])]);
        let state = Rc::new(RefCell::new(RunState::default()));
        let preparation_state = state.clone();
        cx.on_quit_requested(move |cx| {
            let mut state = preparation_state.borrow_mut();
            state.attempts += 1;
            let attempt = state.attempts;
            let heartbeat_start = state.heartbeats;
            let preparation_state = preparation_state.clone();
            eprintln!("prepare {attempt}: started");
            if self_test && attempt == 1 {
                cx.quit();
                cx.quit();
            }
            async move {
                smol::Timer::after(Duration::from_millis(500)).await;
                let allow = attempt > 1;
                let mut state = preparation_state.borrow_mut();
                if self_test {
                    assert!(state.heartbeats > heartbeat_start, "runloop stalled");
                }
                state.completed += 1;
                eprintln!("prepare {attempt}: allow={allow}");
                allow
            }
        })
        .expect("this example requires asynchronous quit support (macOS)");
        let cleanup_state = state.clone();
        cx.on_app_quit(move |_| {
            let mut state = cleanup_state.borrow_mut();
            state.cleanup += 1;
            assert_eq!(state.cleanup, 1);
            eprintln!("on_app_quit: final shutdown (count={})", state.cleanup);
            if self_test {
                assert_eq!(state.attempts, 2);
                assert_eq!(state.completed, 2);
                eprintln!("SELF-TEST PASS: two preparations, responsive runloop, cleanup once");
            }
            async {}
        })
        .detach();
        let heartbeat_state = state.clone();
        cx.spawn(async move |_| {
            loop {
                smol::Timer::after(Duration::from_millis(100)).await;
                heartbeat_state.borrow_mut().heartbeats += 1;
                eprintln!("heartbeat");
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(640.), px(240.)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    show: !self_test,
                    focus: !self_test,
                    ..Default::default()
                },
                |window, cx| {
                    cx.new(|cx| {
                        let focus_handle = cx.focus_handle();
                        focus_handle.focus(window, cx);
                        Example { focus_handle }
                    })
                },
            )
            .unwrap();
        if self_test {
            cx.spawn(async move |cx| {
                smol::Timer::after(Duration::from_millis(300)).await;
                eprintln!("self-test: dispatching Quit through the window");
                window
                    .update(cx, |_, window, cx| {
                        window.dispatch_action(Box::new(Quit), cx);
                        cx.quit();
                        cx.quit();
                    })
                    .unwrap();
                smol::Timer::after(Duration::from_millis(800)).await;
                {
                    let state = state.borrow();
                    assert_eq!(state.attempts, 1, "first quit was not dispatched");
                    assert_eq!(state.completed, 1, "first preparation did not finish");
                    assert_eq!(state.cleanup, 0, "cancelled quit ran final cleanup");
                }
                eprintln!("self-test: cancellation preserved runloop; requesting second quit");
                cx.update(|cx| cx.quit());
                smol::Timer::after(Duration::from_secs(2)).await;
                panic!("self-test: approved quit did not exit the native runloop");
            })
            .detach();
        } else {
            cx.activate(true);
        }
    });
}
