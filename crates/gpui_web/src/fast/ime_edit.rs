//! The span of an IME edit, kept to whole code points.
//!
//! The `input` listener recovers what an IME edit did by diffing the mirror's
//! UTF-16 text before and after it: the units both share at the start (the
//! prefix) and at the end (the suffix) are untouched, and what lies between
//! is the inserted text. Compared unit by unit, the two can share half of a
//! surrogate pair: replacing "😀" (U+1F600, `D83D DE00`) with "😃"
//! (U+1F603, `D83D DE03`) shares the high surrogate, so the inserted text was
//! a lone `DE03`, which `String::from_utf16_lossy` turns into U+FFFD, and the
//! document got "�" where the IME put "😃".

const HIGH_SURROGATES: std::ops::RangeInclusive<u16> = 0xd800..=0xdbff;
const LOW_SURROGATES: std::ops::RangeInclusive<u16> = 0xdc00..=0xdfff;

/// Shortens `prefix_length` and `suffix_length`, the UTF-16 units `new` shares
/// with the text before the edit at its start and its end, so that neither
/// ends inside a surrogate pair of `new`. Both are shared with the old text,
/// so neither splits a pair of the old text either.
pub(crate) fn whole_code_points(
    new: &[u16],
    mut prefix_length: usize,
    mut suffix_length: usize,
) -> (usize, usize) {
    if prefix_length > 0 && HIGH_SURROGATES.contains(&new[prefix_length - 1]) {
        prefix_length -= 1;
    }
    if suffix_length > 0 && LOW_SURROGATES.contains(&new[new.len() - suffix_length]) {
        suffix_length -= 1;
    }
    (prefix_length, suffix_length)
}

#[cfg(test)]
mod tests {
    use super::whole_code_points;

    /// The input listener's diff without its caret anchor: the longest shared
    /// prefix and suffix, the prefix capped so the two don't overlap.
    fn shared_lengths(old: &[u16], new: &[u16]) -> (usize, usize) {
        let suffix = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take_while(|(old, new)| old == new)
            .count();
        let prefix = old
            .iter()
            .zip(new)
            .take_while(|(old, new)| old == new)
            .count()
            .min(old.len() - suffix)
            .min(new.len() - suffix);
        (prefix, suffix)
    }

    /// What the document gets for the edit from `old` to `new`.
    fn inserted(old: &str, new: &str) -> String {
        let old: Vec<u16> = old.encode_utf16().collect();
        let new: Vec<u16> = new.encode_utf16().collect();
        let (prefix, suffix) = shared_lengths(&old, &new);
        let (prefix, suffix) = whole_code_points(&new, prefix, suffix);
        String::from_utf16(&new[prefix..new.len() - suffix]).expect("a split surrogate pair")
    }

    #[test]
    fn an_emoji_replaced_by_one_sharing_its_high_surrogate_is_inserted_whole() {
        // U+1F600 and U+1F603 share the high surrogate D83D.
        assert_eq!(inserted("a😀b", "a😃b"), "😃");
    }

    #[test]
    fn an_emoji_replaced_by_one_sharing_its_low_surrogate_is_inserted_whole() {
        // U+1F600 (D83D DE00) and U+1F200 (D83C DE00) share the low surrogate.
        assert_eq!(inserted("a😀b", "a🈀b"), "🈀");
    }

    #[test]
    fn edits_that_split_no_pair_are_unchanged() {
        assert_eq!(inserted("a😀b", "a😀xb"), "x");
        assert_eq!(inserted("ab", "a😀b"), "😀");
        assert_eq!(inserted("😀", "😀😀"), "😀");
        assert_eq!(inserted("hello", "help"), "p");
        let new: Vec<u16> = "a😀b".encode_utf16().collect();
        assert_eq!(whole_code_points(&new, 1, 1), (1, 1));
        assert_eq!(whole_code_points(&new, 3, 1), (3, 1));
    }
}
