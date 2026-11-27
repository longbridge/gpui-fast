//! Keys that end an input method composition on macOS.
//!
//! With a dead key pending (U.S. input, Option-E), AppKit handles a
//! following Return by committing the marked text and only then calling
//! `doCommandBySelector:`. By that point the input handler has no marked
//! text left, so the keystroke would reach the application as an ordinary
//! Enter (or Cmd-Enter): a chat box would send the unfinished draft
//! (longbridge/gpui-kit#3456). Whether the key began during a composition is
//! therefore captured before AppKit sees the event, and kept for that one
//! event.

use gpui::Keystroke;

/// Whether a key that arrived while text was marked only ends that
/// composition, and so must not also be dispatched to the application.
pub(crate) fn finishes_composition(keystroke: &Keystroke, was_composing: bool) -> bool {
    was_composing && keystroke.key == "enter"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_ends_composition_with_any_modifiers() {
        for source in ["enter", "cmd-enter", "shift-enter", "alt-enter"] {
            let keystroke = Keystroke::parse(source).unwrap();
            assert!(finishes_composition(&keystroke, true), "{source}");
            assert!(!finishes_composition(&keystroke, false), "{source}");
        }
    }

    #[test]
    fn other_keys_reach_the_application() {
        for source in ["escape", "tab", "left", "cmd-a", "backspace"] {
            let keystroke = Keystroke::parse(source).unwrap();
            assert!(!finishes_composition(&keystroke, true), "{source}");
        }
    }
}
