//! Which script a character belongs to. The fetch side and the view have to agree on which lines want a second line, so the ranges live in one place.

/// Scripts that put no spaces between words: kana, CJK ideographs and Hangul.
pub fn is_cjk_char(ch: char) -> bool {
    matches!(ch as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// Scripts whose romanization is a reading rather than a transliteration, so only a provider or a dictionary can supply it: Japanese and Chinese.
///
/// Arabic, Hebrew, Thai and the Indic scripts are left out on purpose. They omit short vowels in writing, so a character map yields "ktb" rather than a word, and no provider here carries readings for them either.
pub fn needs_reading(ch: char) -> bool {
    matches!(ch as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// The scripts a romanization is for: Cyrillic, Hebrew, Arabic, Thai, kana, CJK and Hangul.
pub fn is_non_latin_char(ch: char) -> bool {
    matches!(ch as u32, 0x0400..=0x04FF | 0x0590..=0x05FF | 0x0600..=0x06FF | 0x0E00..=0x0E7F | 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF)
}

pub fn is_kana(ch: char) -> bool {
    matches!(ch as u32, 0x3040..=0x30FF)
}

pub fn is_han(ch: char) -> bool {
    matches!(ch as u32, 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF)
}

/// Whether two adjacent karaoke words need a space between them.
///
/// Only for sources that ship no whitespace of their own. Joining everything with spaces put a gap between every Japanese syllable, and joining with none would run English words together, so it is decided per boundary.
pub fn space_between(left: &str, right: &str) -> bool {
    match (left.chars().next_back(), right.chars().next()) {
        (Some(l), Some(r)) => !(is_cjk_char(l) && is_cjk_char(r)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spaces_go_between_words_but_not_between_syllables() {
        assert!(space_between("hello", "world"));
        assert!(!space_between("千", "本"));
        assert!(!space_between("사랑", "해"));
        assert!(space_between("桜", "night"));
        assert!(space_between("night", "桜"));
        assert!(!space_between("", "a"));
        assert!(!space_between("a", ""));
    }

    #[test]
    fn script_classes() {
        assert!(is_non_latin_char('д') && !needs_reading('д'));
        assert!(is_non_latin_char('한') && !needs_reading('한') && is_cjk_char('한'));
        assert!(needs_reading('桜') && is_han('桜') && !is_kana('桜'));
        assert!(needs_reading('ミ') && is_kana('ミ'));
        assert!(!is_non_latin_char('a') && !is_non_latin_char('é'));
    }
}
