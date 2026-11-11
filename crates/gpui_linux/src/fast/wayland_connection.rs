//! Ends the event loop when the compositor breaks the Wayland connection with
//! a protocol error.
//!
//! A protocol error (`wl_display.error`, which a compositor posts when, for
//! instance, a client binds a `wl_output` removed a moment before by a monitor
//! unplug) never reached the event loop, so the app spun at full CPU instead of
//! quitting. `EventQueue::dispatch_pending` ignores libwayland's error, which
//! wayland-backend logs on every call; `calloop_wayland_source` only reports
//! I/O errors from flushing and reading; and libwayland refuses to prepare a
//! read on a broken connection, so the source asks to be dispatched again
//! without sleeping. A closed socket was already an I/O error and ended the
//! loop. The source's callback now returns the connection's error, as the X11
//! client's does for a lost X server, and `LinuxPlatform::run` quits.

use calloop::{InsertError, LoopHandle, RegistrationToken};
use calloop_wayland_source::WaylandSource;
use wayland_client::{Connection, DispatchError, EventQueue};

/// Registers the connection's event source, dispatching `queue` to `state`.
pub(crate) fn insert_source<State: 'static>(
    handle: &LoopHandle<'static, ()>,
    connection: Connection,
    queue: EventQueue<State>,
    mut state: State,
) -> Result<RegistrationToken, InsertError<WaylandSource<State>>> {
    handle.insert_source(
        WaylandSource::new(connection.clone(), queue),
        move |_, queue, _| dispatch_pending(&connection, queue, &mut state),
    )
}

/// Dispatches the queue's pending events, or returns the error that broke the
/// connection.
pub(crate) fn dispatch_pending<State>(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
) -> Result<usize, DispatchError> {
    let dispatched = queue.dispatch_pending(state)?;
    match connection.backend().last_error() {
        Some(error) => Err(DispatchError::Backend(error)),
        None => Ok(dispatched),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use calloop::EventLoop;
    use wayland_client::protocol::wl_registry::{self, WlRegistry};
    use wayland_client::{Connection, Dispatch, QueueHandle};

    struct State;

    impl Dispatch<WlRegistry, ()> for State {
        fn event(
            _: &mut Self,
            _: &WlRegistry,
            _: wl_registry::Event,
            _: &(),
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
        }
    }

    /// `wl_display.error(wl_registry@2, 0, message)`, as libwayland-server
    /// sends it for a bind to a removed global.
    fn display_error(message: &str) -> Vec<u8> {
        let mut text = message.as_bytes().to_vec();
        text.push(0);
        let length = text.len() as u32;
        text.resize(text.len().next_multiple_of(4), 0);
        let size = 8 + 12 + text.len() as u32;
        let mut event = Vec::new();
        for word in [1, size << 16, 2, 0, length] {
            event.extend_from_slice(&u32::to_ne_bytes(word));
        }
        event.extend_from_slice(&text);
        event
    }

    #[test]
    fn a_protocol_error_ends_the_event_loop() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let connection = Connection::from_socket(client).unwrap();
        let queue = connection.new_event_queue::<State>();
        let _registry = connection.display().get_registry(&queue.handle(), ());
        connection.flush().unwrap();
        server
            .write_all(&display_error("invalid global wl_output (42)"))
            .unwrap();

        let mut event_loop = EventLoop::<()>::try_new().unwrap();
        super::insert_source(&event_loop.handle(), connection, queue, State).unwrap();

        // Before, no dispatch ever returned the error.
        let ended = (0..100).any(|_| {
            event_loop
                .dispatch(Duration::from_millis(10), &mut ())
                .is_err()
        });
        assert!(ended, "the event loop kept running on a broken connection");
        drop(server);
    }
}
