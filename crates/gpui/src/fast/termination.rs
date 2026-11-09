use crate::App;
#[cfg(any(test, feature = "test-support"))]
use crate::{Result, Task};
#[cfg(any(test, feature = "test-support"))]
use std::future::Future;

pub(crate) fn restart_option<T: Clone + Default>(value: &mut T) -> T {
    #[cfg(any(test, feature = "test-support"))]
    {
        value.clone()
    }
    #[cfg(not(any(test, feature = "test-support")))]
    {
        std::mem::take(value)
    }
}

#[cfg(any(test, feature = "test-support"))]
impl App {
    /// Replace the asynchronous quit-request handler on supported platforms (currently macOS).
    ///
    /// Returning false cancels this request and permits a later retry. Returning true allows
    /// final shutdown, including `on_app_quit` observers. The handler runs on the main thread
    /// after the native delegate returns, with no final-shutdown timeout.
    /// Registration during a pending request affects the next request.
    /// Unsupported platforms return an error and retain their existing quit behavior.
    pub fn on_quit_requested<Fut>(
        &mut self,
        mut handler: impl FnMut(&mut App) -> Fut + 'static,
    ) -> Result<()>
    where
        Fut: Future<Output = bool> + 'static,
    {
        let app = self.this.clone();
        let executor = self.foreground_executor.clone();
        self.platform.on_quit_requested(Box::new(move || {
            let Some(app) = app.upgrade() else {
                return Task::ready(true);
            };
            let future = app.borrow_mut().update(|cx| handler(cx));
            executor.spawn(future)
        }))
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn unsupported() -> Result<()> {
    anyhow::bail!("asynchronous quit requests are not supported on this platform")
}

#[cfg(test)]
mod tests {
    use crate::TestAppContext;
    use std::{ffi::OsString, path::PathBuf};

    #[gpui::test]
    async fn restart_retry_preserves_configuration(cx: &mut TestAppContext) {
        #[cfg(unix)]
        let argument = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(b"/tmp/restart data/\xff".to_vec())
        };
        #[cfg(not(unix))]
        let argument = OsString::from("restart data");
        let arguments = vec![OsString::from("--user-data-dir"), argument];
        let path = PathBuf::from("updated-app");
        cx.update(|cx| {
            cx.restart_arguments = arguments.clone();
            cx.set_restart_path(path.clone());
        });

        for _ in 0..2 {
            let restart = cx.expect_restart();
            cx.update(|cx| cx.restart());
            let actual = restart.await.expect("restart was not requested");
            assert_eq!(actual, (Some(path.clone()), arguments.clone()));
        }

        let replacement = PathBuf::from("newer-app");
        cx.update(|cx| cx.set_restart_path(replacement.clone()));
        let restart = cx.expect_restart();
        cx.update(|cx| cx.restart());
        assert_eq!(
            restart.await.expect("restart was not requested"),
            (Some(replacement), arguments)
        );
    }

    #[gpui::test]
    fn unsupported_platform_reports_registration_error(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(
                cx.on_quit_requested(|_| -> std::future::Ready<bool> {
                    panic!("unsupported platforms must not invoke the handler");
                })
                .is_err()
            );
        });
    }
}
