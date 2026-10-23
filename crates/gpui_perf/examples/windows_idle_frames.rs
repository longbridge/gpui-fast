#[cfg(target_os = "windows")]
mod windows {
    use gpui::*;
    use gpui_fast as gpui;
    use gpui_platform_fast as gpui_platform;
    use std::{
        ffi::c_void,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetCurrentThread() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            created: *mut u64,
            exited: *mut u64,
            kernel: *mut u64,
            user: *mut u64,
        ) -> i32;
        fn QueryProcessCycleTime(process: *mut c_void, cycles: *mut u64) -> i32;
        fn QueryThreadCycleTime(thread: *mut c_void, cycles: *mut u64) -> i32;
    }

    fn counters() -> (u64, u64, u64) {
        let (mut created, mut exited, mut kernel, mut user, mut cycles, mut main_cycles) =
            (0, 0, 0, 0, 0, 0);
        unsafe {
            assert_ne!(
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut created,
                    &mut exited,
                    &mut kernel,
                    &mut user
                ),
                0
            );
            assert_ne!(QueryProcessCycleTime(GetCurrentProcess(), &mut cycles), 0);
            assert_ne!(
                QueryThreadCycleTime(GetCurrentThread(), &mut main_cycles),
                0
            );
        }
        (kernel + user, cycles, main_cycles)
    }

    struct IdleView {
        renders: Arc<AtomicUsize>,
        lit: bool,
    }

    impl Render for IdleView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            self.renders.fetch_add(1, Ordering::Relaxed);
            div()
                .size_full()
                .flex()
                .flex_col()
                .p_8()
                .gap_4()
                .font_family("Segoe UI")
                .text_color(rgb(0xe8e8e8))
                .bg(rgb(0x202124))
                .child("GPUI Windows idle benchmark")
                .child(
                    div()
                        .flex()
                        .gap_4()
                        .children(["Participant 1", "Participant 2"].map(|label| {
                            div()
                                .w(px(320.))
                                .h(px(180.))
                                .rounded_lg()
                                .flex()
                                .items_center()
                                .justify_center()
                                .border_2()
                                .border_color(if self.lit {
                                    rgb(0x65b879)
                                } else {
                                    rgb(0x404246)
                                })
                                .bg(rgb(0x303236))
                                .child(label)
                        })),
                )
        }
    }

    fn chain(window: &Window, completed: Arc<AtomicUsize>, remaining: usize) {
        window.on_next_frame(move |window, _| {
            completed.fetch_add(1, Ordering::Relaxed);
            if remaining > 1 {
                chain(window, completed, remaining - 1);
            }
        });
    }

    pub fn run() {
        let output =
            std::path::PathBuf::from(std::env::args_os().nth(1).expect("output JSON path"));
        let verify = std::env::args().any(|arg| arg == "--verify");
        unsafe {
            std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "true");
        }
        gpui_platform::application().run(move |cx: &mut App| {
            let renders = Arc::new(AtomicUsize::new(0));
            let view_renders = renders.clone();
            let handle = cx
                .open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                            point(px(100.), px(100.)),
                            size(px(1280.), px(800.)),
                        ))),
                        ..Default::default()
                    },
                    |_, cx| {
                        cx.new(|_| IdleView {
                            renders: view_renders,
                            lit: true,
                        })
                    },
                )
                .unwrap();
            cx.spawn(async move |cx| {
                cx.background_executor().timer(Duration::from_secs(3)).await;
                let before = counters();
                let before_renders = renders.load(Ordering::Relaxed);
                let start = Instant::now();
                cx.background_executor()
                    .timer(Duration::from_secs(20))
                    .await;
                let elapsed = start.elapsed().as_secs_f64();
                let after = counters();
                let idle_renders = renders.load(Ordering::Relaxed) - before_renders;
                let mut callbacks = 0;
                let mut update_renders = 0;
                if verify {
                    let second_renders = Arc::new(AtomicUsize::new(0));
                    let other_renders = second_renders.clone();
                    let second = cx.update(|cx| {
                        cx.open_window(WindowOptions::default(), |_, cx| {
                            cx.new(|_| IdleView {
                                renders: other_renders,
                                lit: true,
                            })
                        })
                        .unwrap()
                    });
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let old_renders =
                        renders.load(Ordering::Relaxed) + second_renders.load(Ordering::Relaxed);
                    for frame in 0..25 {
                        for target in [handle, second] {
                            target
                                .update(cx, |view, _, cx| {
                                    view.lit = frame % 2 == 0;
                                    cx.notify();
                                })
                                .unwrap();
                        }
                        cx.background_executor()
                            .timer(Duration::from_millis(80))
                            .await;
                    }
                    update_renders = renders.load(Ordering::Relaxed)
                        + second_renders.load(Ordering::Relaxed)
                        - old_renders;
                    let completed = Arc::new(AtomicUsize::new(0));
                    for target in [handle, second] {
                        let completed = completed.clone();
                        target
                            .update(cx, |_, window, _| chain(window, completed, 60))
                            .unwrap();
                    }
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                    callbacks = completed.load(Ordering::Relaxed);
                    second
                        .update(cx, |_, window, _| window.remove_window())
                        .unwrap();
                }
                let result = serde_json::json!({
                    "elapsed_seconds": elapsed,
                    "process_cpu_ms": (after.0 - before.0) as f64 / 10000.,
                    "process_cycles": after.1 - before.1,
                    "main_thread_cycles": after.2 - before.2,
                    "idle_renders": idle_renders,
                    "verify": verify,
                    "update_renders": update_renders,
                    "chained_callbacks": callbacks,
                });
                std::fs::write(output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
                assert_eq!(idle_renders, 0);
                if verify {
                    assert!(update_renders >= 50);
                    assert_eq!(callbacks, 120);
                }
                cx.update(|cx| cx.quit());
            })
            .detach();
        });
    }
}

fn main() {
    #[cfg(target_os = "windows")]
    windows::run();
    #[cfg(not(target_os = "windows"))]
    eprintln!("This benchmark requires Windows.");
}
