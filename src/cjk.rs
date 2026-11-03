//! Which language a text's CJK characters are in, for the glyph shapes they take: Chinese,
//! Japanese and Korean share the Han characters in Unicode but draw many of them differently,
//! and a font left to choose picks one for all (here Korean, for Japanese text too). As the
//! user's Japanese typing app does on the web, a text is set in the CJK family of its language
//! (`Theme.cjk-family`), and tagged with it for its spacing: kana make it Japanese, Hangul
//! Korean, and Han characters alone take the language the user chose (see
//! `Settings::han_language`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CjkLanguage {
    Japanese,
    SimplifiedChinese,
    TraditionalChinese,
    Korean,
}

/// The CJK characters of a text: their language, and how they stand in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CjkText {
    pub(crate) language: CjkLanguage,
    pub(crate) standing: Standing,
}

/// Whether CJK is a text's main script, which sets the whole text in its family and line
/// height, or its characters are guests in another script's text, which keeps its own: a
/// guest doesn't change the setting (JLReq).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Standing {
    Main,
    Guest,
}

impl CjkText {
    /// The CJK characters in `text`, if it has any: Japanese with kana among them, Korean
    /// with Hangul, `han` with Han characters alone. They are the main script with at least
    /// half as many as other letters (a CJK character carries about what a short word does),
    /// and guests with fewer, as in an English text that quotes a word. Code, between
    /// backticks, doesn't count.
    pub(crate) fn of(text: &str, han: CjkLanguage) -> Option<Self> {
        let mut has_kana = false;
        let mut has_hangul = false;
        let mut cjk_characters = 0_usize;
        let mut other_letters = 0_usize;
        for character in outside_code(text) {
            if is_kana(character) {
                has_kana = true;
                cjk_characters += 1;
            } else if is_hangul(character) {
                has_hangul = true;
                cjk_characters += 1;
            } else if is_han(character) {
                cjk_characters += 1;
            } else if character.is_alphabetic() {
                other_letters += 1;
            }
        }
        if cjk_characters == 0 {
            return None;
        }
        let language = if has_kana {
            CjkLanguage::Japanese
        } else if has_hangul {
            CjkLanguage::Korean
        } else {
            han
        };
        let standing = if cjk_characters * 2 < other_letters {
            Standing::Guest
        } else {
            Standing::Main
        };
        Some(Self { language, standing })
    }
}

impl CjkLanguage {
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
}

/// The characters of `text` outside code, which Markdown sets between runs of backticks.
fn outside_code(text: &str) -> impl Iterator<Item = char> + '_ {
    let mut in_code = false;
    let mut after_backtick = false;
    text.chars().filter(move |&character| {
        let backtick = character == '`';
        if backtick && !after_backtick {
            in_code = !in_code;
        }
        after_backtick = backtick;
        !backtick && !in_code
    })
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
        let language = |text| CjkText::of(text, han).map(|cjk| cjk.language);
        assert_eq!(
            language("このテストは CI でだけ失敗します"),
            Some(CjkLanguage::Japanese)
        );
        assert_eq!(language("ﾃｽﾄ"), Some(CjkLanguage::Japanese));
        assert_eq!(
            language("테스트가 실패합니다 (失敗)"),
            Some(CjkLanguage::Korean)
        );
        assert_eq!(language("測試失敗"), Some(han));
        // A guest keeps its language, for its glyph shapes.
        assert_eq!(
            language("The word テスト means test, as you may know"),
            Some(CjkLanguage::Japanese)
        );
        assert_eq!(language("cargo test fails"), None);
        assert_eq!(language("「」。、"), None);
    }

    /// An English text quoting a CJK word keeps its own setting; a Japanese one is set as
    /// Japanese however much code it holds.
    #[test]
    fn cjk_is_the_main_script_by_its_share_of_letters_outside_code() {
        let han = CjkLanguage::SimplifiedChinese;
        let standing = |text| CjkText::of(text, han).map(|cjk| cjk.standing);
        assert_eq!(
            standing("Use the 漢字 field for names"),
            Some(Standing::Guest)
        );
        // Half as many CJK characters as other letters is where they become the main script.
        assert_eq!(standing("漢字漢字漢 abcdefghijk"), Some(Standing::Guest));
        assert_eq!(standing("漢字漢字漢字 abcdefghijkl"), Some(Standing::Main));
        assert_eq!(
            standing("`cargo test --workspace` を実行すると `src/main.rs` のテストが失敗します"),
            Some(Standing::Main)
        );
        assert_eq!(
            standing(
                "テストが失敗します:\n```rust\nfn main() { println!(\"hello world, this is long\"); }\n```"
            ),
            Some(Standing::Main)
        );
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
