//! Which language a text's CJK characters are in, for the glyph shapes they take: Chinese,
//! Japanese and Korean share the Han characters in Unicode but draw many of them differently,
//! and a font left to choose picks one for all (here Korean, for Japanese text too). As the
//! user's Japanese typing app does on the web, a text is set in the CJK family of its language:
//! kana make it Japanese, Hangul Korean, and Han characters alone take the language the user
//! chose (see `Settings::han_language`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CjkLanguage {
    Japanese,
    SimplifiedChinese,
    TraditionalChinese,
    Korean,
}

impl CjkLanguage {
    /// The language of the CJK characters in `text`: Japanese with kana in it, Korean with
    /// Hangul, `han` with Han characters alone; none without any.
    pub(crate) fn of_text(text: &str, han: Self) -> Option<Self> {
        let mut has_hangul = false;
        let mut has_han = false;
        for character in text.chars() {
            if is_kana(character) {
                return Some(Self::Japanese);
            }
            has_hangul |= is_hangul(character);
            has_han |= is_han(character);
        }
        if has_hangul {
            Some(Self::Korean)
        } else {
            has_han.then_some(han)
        }
    }

    /// The CJK language a locale ("ja_JP.UTF-8", "zh_TW") names, if it names one.
    pub(crate) fn of_locale(locale: &str) -> Option<Self> {
        let locale = locale.to_ascii_lowercase().replace('-', "_");
        let traditional = ["zh_tw", "zh_hk", "zh_mo", "zh_hant"];
        if locale.starts_with("ja") {
            Some(Self::Japanese)
        } else if locale.starts_with("ko") {
            Some(Self::Korean)
        } else if traditional.iter().any(|prefix| locale.starts_with(prefix)) {
            Some(Self::TraditionalChinese)
        } else if locale.starts_with("zh") {
            Some(Self::SimplifiedChinese)
        } else {
            None
        }
    }

    /// The desktop's language, if it is a CJK one: the first locale set of `LC_ALL`,
    /// `LC_MESSAGES` and `LANG`.
    pub(crate) fn of_desktop() -> Option<Self> {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .find(|value| !value.is_empty())
            .and_then(|locale| Self::of_locale(&locale))
    }

    /// The family of Noto Sans CJK that draws this language's glyph shapes.
    pub(crate) fn family(self) -> &'static str {
        match self {
            Self::Japanese => "Noto Sans CJK JP",
            Self::SimplifiedChinese => "Noto Sans CJK SC",
            Self::TraditionalChinese => "Noto Sans CJK TC",
            Self::Korean => "Noto Sans CJK KR",
        }
    }
}

/// Hiragana, katakana (also half-width) and their extensions: only Japanese writes them.
fn is_kana(character: char) -> bool {
    matches!(
        character,
        '\u{3040}'..='\u{30FF}'
            | '\u{31F0}'..='\u{31FF}'
            | '\u{FF66}'..='\u{FF9F}'
            | '\u{1B000}'..='\u{1B16F}'
    )
}

/// Hangul syllables and jamo (also half-width): only Korean writes them.
fn is_hangul(character: char) -> bool {
    matches!(
        character,
        '\u{1100}'..='\u{11FF}'
            | '\u{3130}'..='\u{318F}'
            | '\u{A960}'..='\u{A97F}'
            | '\u{AC00}'..='\u{D7FF}'
            | '\u{FFA0}'..='\u{FFDC}'
    )
}

/// Han characters (CJK ideographs, their extensions and compatibility forms), which Chinese,
/// Japanese and Korean share.
fn is_han(character: char) -> bool {
    matches!(
        character,
        '\u{3400}'..='\u{4DBF}'
            | '\u{4E00}'..='\u{9FFF}'
            | '\u{F900}'..='\u{FAFF}'
            | '\u{20000}'..='\u{323AF}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kana make a text Japanese even among Han characters, Hangul Korean, and Han alone takes
    /// the language chosen; Latin text and CJK punctuation alone have none.
    #[test]
    fn a_texts_language_comes_from_its_script() {
        let han = CjkLanguage::TraditionalChinese;
        assert_eq!(
            CjkLanguage::of_text("このテストは CI でだけ失敗します", han),
            Some(CjkLanguage::Japanese)
        );
        assert_eq!(
            CjkLanguage::of_text("ﾃｽﾄ", han),
            Some(CjkLanguage::Japanese)
        );
        assert_eq!(
            CjkLanguage::of_text("테스트가 실패합니다 (失敗)", han),
            Some(CjkLanguage::Korean)
        );
        assert_eq!(CjkLanguage::of_text("測試失敗", han), Some(han));
        assert_eq!(CjkLanguage::of_text("cargo test fails", han), None);
        assert_eq!(CjkLanguage::of_text("「」。、", han), None);
    }

    #[test]
    fn a_locale_names_its_cjk_language() {
        assert_eq!(
            CjkLanguage::of_locale("ja_JP.UTF-8"),
            Some(CjkLanguage::Japanese)
        );
        assert_eq!(
            CjkLanguage::of_locale("zh_TW.UTF-8"),
            Some(CjkLanguage::TraditionalChinese)
        );
        assert_eq!(
            CjkLanguage::of_locale("zh-Hant"),
            Some(CjkLanguage::TraditionalChinese)
        );
        assert_eq!(
            CjkLanguage::of_locale("zh_CN.UTF-8"),
            Some(CjkLanguage::SimplifiedChinese)
        );
        assert_eq!(CjkLanguage::of_locale("ko_KR"), Some(CjkLanguage::Korean));
        assert_eq!(CjkLanguage::of_locale("de_DE.UTF-8"), None);
        assert_eq!(CjkLanguage::of_locale("en_GB.UTF-8"), None);
    }
}
