//! XIM input contexts on X11 (zed-industries/zed#59662).
//!
//! An input context belongs to an opened input method, so `CreateIc` is only
//! valid once the server's `OpenReply` has assigned the input method's id.
//! `XimHandler`'s handshake creates an input context itself when that reply
//! arrives, for the window focus last went to, so a window focused while the
//! handshake is still running leaves the input context to it. Creating one
//! from `FocusIn` before then names input method 0: fcitx5 answers with an
//! `XIM_ERROR` the client cannot parse, and the client drops its XIM
//! connection for the rest of the session, leaving every window without an
//! input method.

use crate::linux::XimHandler;

/// Whether the XIM handshake has created its input context, so that a focused
/// window may create its own.
pub(crate) fn is_handshake_complete(handler: Option<&XimHandler>) -> bool {
    handler.is_some_and(|handler| handler.connected)
}

#[cfg(test)]
mod tests {
    use super::is_handshake_complete;
    use crate::linux::XimHandler;

    #[test]
    fn focus_waits_for_the_handshake() {
        assert!(!is_handshake_complete(None));
        assert!(!is_handshake_complete(Some(&XimHandler::new())));
    }

    #[test]
    fn focus_creates_an_input_context_after_the_handshake() {
        let mut handler = XimHandler::new();
        handler.connected = true;
        assert!(is_handshake_complete(Some(&handler)));
    }
}
