//! Holding the CPU's clock up while `--auto` measures, on macOS.
//!
//! macOS clocks a core by how busy it keeps, and runs a thread that leaves
//! most of each frame idle on efficiency cores or at a low clock. The less a
//! frame does, the longer each of its instructions then takes: CPU time per
//! frame doubled from one run of the same build to the next, and retained
//! views measured as slow as drawing from scratch while retiring 40% fewer
//! instructions. A helper process spinning on a performance core holds that
//! core cluster's clock up for the whole run, as the performance governor
//! does on Linux, so that CPU time measures the work done. It is a process of
//! its own so that its CPU is not counted in the process measured.

use smol::process::{Child, Command};

/// The flag the helper process is started with.
pub const HOLD_CLOCK_FLAG: &str = "--hold-clock";

/// The helper process, killed when dropped or by [`ClockHold::stop`].
pub struct ClockHold(Option<Child>);

impl ClockHold {
    /// Starts the helper where the platform needs one, unless
    /// `--no-hold-clock` was given.
    pub fn start() -> Self {
        if !cfg!(target_os = "macos") || std::env::args().any(|arg| arg == "--no-hold-clock") {
            return Self(None);
        }
        let child = std::env::current_exe()
            .and_then(|exe| {
                Command::new(exe)
                    .arg(HOLD_CLOCK_FLAG)
                    .kill_on_drop(true)
                    .spawn()
            })
            .inspect_err(|error| eprintln!("could not start the clock holder: {error}"))
            .ok();
        Self(child)
    }

    /// Whether the helper is running.
    pub fn is_holding(&self) -> bool {
        self.0.is_some()
    }

    pub fn stop(&mut self) {
        self.0.take();
    }
}

/// The helper process: spins at the main thread's quality of service, which
/// macOS runs on performance cores, until its parent exits.
pub fn hold() -> ! {
    #[cfg(target_os = "macos")]
    {
        const QOS_CLASS_USER_INTERACTIVE: libc::c_uint = 0x21;
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(
                qos_class: libc::c_uint,
                relative_priority: libc::c_int,
            ) -> libc::c_int;
        }
        // SAFETY: sets the calling thread's own quality of service.
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) };
    }
    // SAFETY: `getppid` only reads the parent's id.
    let parent = unsafe { libc::getppid() };
    let mut value = 0u64;
    loop {
        for _ in 0..1_000_000 {
            value = std::hint::black_box(value.wrapping_mul(6364136223846793005).wrapping_add(1));
        }
        // An orphan is adopted by launchd: the parent exited without stopping
        // the helper.
        // SAFETY: as above.
        if unsafe { libc::getppid() } != parent {
            std::process::exit(0);
        }
    }
}
