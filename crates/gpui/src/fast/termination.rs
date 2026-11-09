use crate::{App, Result, Task};
use std::future::Future;

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

pub(crate) fn unsupported() -> Result<()> {
    anyhow::bail!("asynchronous quit requests are not supported on this platform")
}

#[cfg(test)]
mod tests {
    use crate::TestAppContext;

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
