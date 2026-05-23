use std::sync::LazyLock;

use super::word_list::WordList;

/// Word lists used to build pet-style names (e.g. "Happily Fluffy Cat").
struct NameParts {
    nouns: WordList,
    nouns_plural: WordList,
    adjectives: WordList,
    adverbs: WordList,
}

/// Configuration for pet name generation.
pub struct NameConfig {
    /// Number of words in the generated name (1 = noun, 2 = adjective + noun,
    /// 3+ = adverbs + adjective + noun).
    pub parts: u8,
}

static NAME_PARTS: LazyLock<NameParts> = LazyLock::new(|| NameParts {
    nouns: WordList::new(include_str!("../../names/pets/nouns.txt")),
    nouns_plural: WordList::new(include_str!("../../names/pets/nouns_plural.txt")),
    adjectives: WordList::new(include_str!("../../names/pets/adjectives.txt")),
    adverbs: WordList::new(include_str!("../../names/pets/adverbs.txt")),
});

/// Generates a random pet-style name with the configured number of parts.
///
/// Returns the name as a vector of word parts, ordered from modifier to noun.
pub fn pet_name(config: &NameConfig) -> Vec<&'static str> {
    pet_name_inner(config, false)
}

/// Same as [`pet_name`] but draws the trailing noun from the pre-pluralized
/// word list.
pub fn pet_name_plural(config: &NameConfig) -> Vec<&'static str> {
    pet_name_inner(config, true)
}

fn pet_name_inner(config: &NameConfig, plural_noun: bool) -> Vec<&'static str> {
    let name_parts = &*NAME_PARTS;
    let nouns = if plural_noun {
        &name_parts.nouns_plural
    } else {
        &name_parts.nouns
    };

    (0..config.parts)
        .rev()
        .map(|i| match i {
            0 => nouns.random_choice(),
            1 => name_parts.adjectives.random_choice(),
            _ => name_parts.adverbs.random_choice(),
        })
        .collect()
}
