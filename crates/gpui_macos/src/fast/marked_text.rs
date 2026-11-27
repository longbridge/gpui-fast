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

use std::cell::Cell;

use gpui::Keystroke;

thread_local! {
    /// Whether the key AppKit is handling began while text was marked.
    /// Key events arrive on the main thread, one at a time.
    static COMPOSING: Cell<bool> = const { Cell::new(false) };
}

/// Notes, before a key goes to the input context, whether text is marked.
pub(crate) fn begin_key(is_composing: bool) {
    COMPOSING.set(is_composing);
}

/// Called from `doCommandBySelector:`: whether the key only ends the
/// composition it began in. If so, the key is marked handled so it is not
/// dispatched to the application.
pub(crate) fn finishes(keystroke: Option<&Keystroke>, handled: &mut Option<bool>) -> bool {
    let was_composing = COMPOSING.replace(false);
    let finishes = keystroke.is_some_and(|keystroke| ends_composition(keystroke, was_composing));
    if finishes {
        *handled = Some(true);
    }
    finishes
}

fn ends_composition(keystroke: &Keystroke, was_composing: bool) -> bool {
    was_composing && keystroke.key == "enter"
}

#[cfg(test)]
mod tests {
    use super::{begin_key, ends_composition, finishes};
    use gpui::Keystroke;

    #[test]
    fn enter_ends_composition_with_any_modifiers() {
        for source in ["enter", "cmd-enter", "shift-enter", "alt-enter"] {
            let keystroke = Keystroke::parse(source).unwrap();
            assert!(ends_composition(&keystroke, true), "{source}");
            assert!(!ends_composition(&keystroke, false), "{source}");
        }
    }

    #[test]
    fn other_keys_reach_the_application() {
        for source in ["escape", "tab", "left", "cmd-a", "backspace"] {
            let keystroke = Keystroke::parse(source).unwrap();
            assert!(!ends_composition(&keystroke, true), "{source}");
        }
    }

    #[test]
    fn composition_state_lasts_one_key() {
        let enter = Keystroke::parse("cmd-enter").unwrap();
        let mut handled = None;
        begin_key(true);
        assert!(finishes(Some(&enter), &mut handled));
        assert_eq!(handled, Some(true));

        let mut handled = None;
        assert!(!finishes(Some(&enter), &mut handled));
        assert_eq!(handled, None);

        begin_key(false);
        assert!(!finishes(Some(&enter), &mut handled));
        assert_eq!(handled, None);
    }
}
