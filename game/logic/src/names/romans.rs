use std::sync::LazyLock;

use super::word_list::WordList;

/// Word lists for the three components of a Roman name (tria nomina).
struct NameParts {
    praenomen: WordList,
    nomen: WordList,
    cognomen: WordList,
    cognomen_plural: WordList,
}

/// Configuration for Roman name generation.
pub struct NameConfig {
    /// Whether to include the praenomen (personal/first name).
    /// When `false`, only the nomen and cognomen are used.
    pub praenomen: bool,
}

static NAME_PARTS: LazyLock<NameParts> = LazyLock::new(|| NameParts {
    praenomen: WordList::new(include_str!("../../names/romans/praenomen.txt")),
    nomen: WordList::new(include_str!("../../names/romans/nomen.txt")),
    cognomen: WordList::new(include_str!("../../names/romans/cognomen.txt")),
    cognomen_plural: WordList::new(include_str!("../../names/romans/cognomen_plural.txt")),
});

/// Generates a random Roman-style name.
///
/// Returns a vector of name parts: `[praenomen, nomen, cognomen]` if
/// `config.praenomen` is `true`, otherwise `[nomen, cognomen]`.
pub fn roman_name(config: &NameConfig) -> Vec<&'static str> {
    roman_name_inner(config, false)
}

/// Same as [`roman_name`] but draws the trailing cognomen from the
/// pre-pluralized list (Latin plurals — e.g. "Aurelii", "Cicerones").
pub fn roman_name_plural(config: &NameConfig) -> Vec<&'static str> {
    roman_name_inner(config, true)
}

fn roman_name_inner(config: &NameConfig, plural_cognomen: bool) -> Vec<&'static str> {
    let name_parts = &*NAME_PARTS;
    let nomen = name_parts.nomen.random_choice();
    let cognomen = if plural_cognomen {
        name_parts.cognomen_plural.random_choice()
    } else {
        name_parts.cognomen.random_choice()
    };

    if config.praenomen {
        let praenomen = name_parts.praenomen.random_choice();
        vec![praenomen, nomen, cognomen]
    } else {
        vec![nomen, cognomen]
    }
}
