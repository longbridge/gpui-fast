use crate::window::Frame;

pub(crate) fn finish(frame: &mut Frame, previous: &mut Frame) {
    if previous.element_states.is_empty() {
        return;
    }

    for key in &frame.accessed_element_states {
        if let Some((key, state)) = previous.element_states.remove_entry(key) {
            frame.element_states.insert(key, state);
            if previous.element_states.is_empty() {
                break;
            }
        }
    }
}
