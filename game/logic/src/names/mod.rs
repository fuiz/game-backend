//! Player name management and validation
//!
//! This module handles the assignment and validation of player names within
//! a game session. It ensures name uniqueness, filters inappropriate content,
//! and maintains bidirectional mappings between player IDs and names.

mod pets;
mod romans;
mod word_list;

use std::collections::{HashMap, HashSet, hash_map::Entry};

use heck::ToTitleCase;
use rustc_hash::FxHashMap;
use rustrict::CensorStr;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{game::Profanity, watcher::Id};

/// Defines the style of automatically generated player names
///
/// When random names are enabled, this enum determines what type of
/// names are generated for players who don't choose their own names.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, garde::Validate)]
#[garde(context(crate::settings::Settings))]
pub enum NameStyle {
    /// Roman-style names (praenomen + nomen, optionally + cognomen)
    Roman(#[garde(range(min = 2, max = 3))] usize),
    /// Pet-style names (adjective + animal combinations)
    Petname(#[garde(range(min = 2, max = 3))] usize),
}

impl Default for NameStyle {
    /// Default name style is Petname with 2 words
    fn default() -> Self {
        Self::Petname(2)
    }
}

impl NameStyle {
    /// Generates a random name according to this style
    ///
    /// # Returns
    ///
    /// A randomly generated name as a String.
    pub fn get_name(&self) -> String {
        match self {
            Self::Roman(count) => romans::roman_name(&romans::NameConfig { praenomen: *count > 2 }),
            Self::Petname(count) => pets::pet_name(&pets::NameConfig { parts: *count as u8 }),
        }
        .join(" ")
        .to_title_case()
    }
}

/// Trait for generating names according to a specific naming scheme.
///
/// Implementors of this trait provide a method to generate and return a name
/// based on their scheme.
pub trait NamingScheme {
    /// Generates and returns a name according to the naming scheme.
    fn get_name(&self) -> String;

    /// Generates and returns the plural form of a name according to the naming scheme.
    ///
    /// # Returns
    ///
    /// A pluralized version of the generated name as a String.
    fn get_plural_name(&self) -> String {
        pluralizer::pluralize(&self.get_name(), 3, false)
    }
}

impl NamingScheme for NameStyle {
    fn get_name(&self) -> String {
        self.get_name()
    }
}

/// Serialization helper for Names struct. Only compiled when persistence is
/// enabled, since `Names`' `Deserialize` (via `serde(from = ...)`)
/// references it.
#[cfg(feature = "serializable")]
#[derive(Deserialize)]
struct NamesSerde {
    mapping: FxHashMap<Id, String>,
}

/// Manages player names and their associations with player IDs
///
/// This struct maintains a bidirectional mapping between player IDs and names,
/// ensuring that names are unique within a game session and meet content
/// and length requirements.
#[derive(Debug, Default, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serializable", serde(from = "NamesSerde"))]
pub struct Names {
    /// Primary mapping from player ID to name
    mapping: FxHashMap<Id, String>,

    /// Reverse mapping from name to player ID (not serialized)
    #[cfg_attr(feature = "serializable", serde(skip_serializing))]
    reverse_mapping: HashMap<String, Id>,
    /// Set of all existing names for quick uniqueness checks (not serialized)
    #[cfg_attr(feature = "serializable", serde(skip_serializing))]
    existing: HashSet<String>,
}

#[cfg(feature = "serializable")]
impl From<NamesSerde> for Names {
    /// Reconstructs the Names struct from serialized data
    ///
    /// This rebuilds the reverse mapping and existing names set from
    /// the primary mapping, which is necessary since these fields
    /// are not serialized.
    fn from(serde: NamesSerde) -> Self {
        let NamesSerde { mapping } = serde;
        let mut reverse_mapping = HashMap::new();
        let mut existing = HashSet::new();
        for (id, name) in &mapping {
            reverse_mapping.insert(name.to_owned(), *id);
            existing.insert(name.to_owned());
        }
        Self {
            mapping,
            reverse_mapping,
            existing,
        }
    }
}

/// Errors that can occur during name validation and assignment
#[derive(Error, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The requested name is already in use by another player
    #[error("name already in-use")]
    Used,
    /// The player already has an assigned name
    #[error("player has an existing name")]
    Assigned,
    /// The name is empty or contains only whitespace
    #[error("name cannot be empty")]
    Empty,
    /// The name contains inappropriate content
    #[error("name is inappropriate")]
    Sinful,
    /// The name exceeds the maximum allowed length
    #[error("name is too long")]
    TooLong,
}

impl Names {
    /// Retrieves the name associated with a player ID
    ///
    /// # Arguments
    ///
    /// * `id` - The player ID to look up
    ///
    /// # Returns
    ///
    /// The player's name if they have one assigned, otherwise `None`
    pub fn get_name(&self, id: &Id) -> Option<&str> {
        self.mapping.get(id).map(String::as_str)
    }

    /// Retrieves the name associated with a player ID, or "Unknown" if not found
    ///
    /// # Arguments
    ///
    /// * `id` - The player ID to look up
    ///
    /// # Returns
    ///
    /// The player's name if they have one assigned, otherwise "Unknown"
    pub fn get_name_or_unknown(&self, id: &Id) -> &str {
        self.get_name(id).unwrap_or("Unknown")
    }

    /// Assigns a name to a player after validation
    ///
    /// This method performs comprehensive validation including length limits,
    /// content filtering, uniqueness checking, and ensures the player doesn't
    /// already have a name assigned.
    ///
    /// # Arguments
    ///
    /// * `id` - The player ID to assign the name to
    /// * `name` - The requested name (will be trimmed of whitespace)
    /// * `profanity` - When [`Profanity::Censor`], reject names that the
    ///   rustrict profanity filter flags as inappropriate; with
    ///   [`Profanity::Allow`] the check is skipped entirely.
    ///
    /// # Returns
    ///
    /// The cleaned and assigned name on success, or an error describing
    /// why the name was rejected.
    ///
    /// # Errors
    ///
    /// * `Error::TooLong` - Name exceeds 30 characters
    /// * `Error::Empty` - Name is empty after trimming whitespace
    /// * `Error::Sinful` - Name contains inappropriate content
    /// * `Error::Used` - Name is already taken by another player
    /// * `Error::Assigned` - Player already has a name assigned
    pub fn set_name<'a>(&mut self, id: Id, name: &'a str, profanity: Profanity) -> Result<&'a str, Error> {
        if name.len() > 30 {
            return Err(Error::TooLong);
        }
        let name = rustrict::trim_whitespace(name);
        if name.is_empty() {
            return Err(Error::Empty);
        }
        if matches!(profanity, Profanity::Censor) && name.is_inappropriate() {
            return Err(Error::Sinful);
        }
        if !self.existing.insert(name.to_owned()) {
            return Err(Error::Used);
        }
        match self.mapping.entry(id) {
            Entry::Occupied(_) => Err(Error::Assigned),
            Entry::Vacant(v) => {
                v.insert(name.to_owned());
                self.reverse_mapping.insert(name.to_owned(), id);
                Ok(name)
            }
        }
    }

    /// Retrieves the player ID associated with a name
    ///
    /// # Arguments
    ///
    /// * `name` - The name to look up
    ///
    /// # Returns
    ///
    /// The player ID if the name is assigned, otherwise `None`
    pub fn get_id(&self, name: &str) -> Option<Id> {
        self.reverse_mapping.get(name).copied()
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_names_set_and_get() {
        let mut names = Names::default();
        let id = Id::new();

        let result = names.set_name(id, "TestPlayer", Profanity::Censor);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "TestPlayer");

        assert_eq!(names.get_name(&id), Some("TestPlayer"));
        assert_eq!(names.get_id("TestPlayer"), Some(id));
    }

    #[test]
    fn test_names_too_long() {
        let mut names = Names::default();
        let id = Id::new();

        let long_name = "a".repeat(31);
        let result = names.set_name(id, &long_name, Profanity::Censor);
        assert_eq!(result, Err(Error::TooLong));
    }

    #[test]
    fn test_names_max_length_allowed() {
        let mut names = Names::default();
        let id = Id::new();

        let max_name = "a".repeat(30);
        let result = names.set_name(id, &max_name, Profanity::Censor);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), max_name);
    }

    #[test]
    fn test_names_empty_name() {
        let mut names = Names::default();
        let id = Id::new();

        assert_eq!(names.set_name(id, "", Profanity::Censor), Err(Error::Empty));
        assert_eq!(names.set_name(id, "   ", Profanity::Censor), Err(Error::Empty));
        assert_eq!(names.set_name(id, "\t\n", Profanity::Censor), Err(Error::Empty));
    }

    #[test]
    fn test_names_whitespace_trimming() {
        let mut names = Names::default();
        let id = Id::new();

        let result = names.set_name(id, "  TestPlayer  ", Profanity::Censor);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "TestPlayer");

        assert_eq!(names.get_name(&id), Some("TestPlayer"));
    }

    #[test]
    fn test_names_duplicate_error() {
        let mut names = Names::default();
        let id1 = Id::new();
        let id2 = Id::new();
        let id3 = Id::new();

        names.set_name(id1, "Player", Profanity::Censor).unwrap();
        let result = names.set_name(id2, "Player", Profanity::Censor);
        assert_eq!(result, Err(Error::Used));

        // Test that whitespace-trimmed names are also considered duplicates
        let result_with_whitespace = names.set_name(id3, "  Player  ", Profanity::Censor);
        assert_eq!(result_with_whitespace, Err(Error::Used));
    }

    #[test]
    fn test_names_already_assigned_error() {
        let mut names = Names::default();
        let id = Id::new();

        names.set_name(id, "FirstName", Profanity::Censor).unwrap();
        let result = names.set_name(id, "SecondName", Profanity::Censor);
        assert_eq!(result, Err(Error::Assigned));

        // Original name should still be there
        assert_eq!(names.get_name(&id), Some("FirstName"));
    }

    #[test]
    fn test_names_inappropriate_content() {
        let mut names = Names::default();
        let id = Id::new();

        // Test some inappropriate words that rustrict should catch
        let inappropriate_names = ["damn", "fuck", "shit"];

        for name in inappropriate_names {
            let result = names.set_name(id, name, Profanity::Censor);
            assert_eq!(
                result,
                Err(Error::Sinful),
                "Expected '{name}' to be flagged as inappropriate"
            );
        }
    }

    #[test]
    fn test_names_get_nonexistent() {
        let names = Names::default();
        let id = Id::new();

        assert_eq!(names.get_name(&id), None);
        assert_eq!(names.get_id("NonexistentPlayer"), None);
    }

    #[cfg(feature = "serializable")]
    #[test]
    fn test_names_serialization_deserialization() {
        let mut original = Names::default();
        let id1 = Id::new();
        let id2 = Id::new();

        original.set_name(id1, "Player1", Profanity::Censor).unwrap();
        original.set_name(id2, "Player2", Profanity::Censor).unwrap();

        // Serialize
        let serialized = serde_json::to_string(&original).unwrap();

        // Deserialize
        let deserialized: Names = serde_json::from_str(&serialized).unwrap();

        // Check that all data is preserved
        assert_eq!(deserialized.get_name(&id1), Some("Player1"));
        assert_eq!(deserialized.get_name(&id2), Some("Player2"));
        assert_eq!(deserialized.get_id("Player1"), Some(id1));
        assert_eq!(deserialized.get_id("Player2"), Some(id2));
    }

    #[cfg(feature = "serializable")]
    #[test]
    fn test_names_reverse_mapping_rebuild() {
        let mut original = Names::default();
        let id = Id::new();
        original.set_name(id, "TestPlayer", Profanity::Censor).unwrap();

        // Serialize and deserialize to test reverse mapping rebuild
        let serialized = serde_json::to_string(&original).unwrap();
        let deserialized: Names = serde_json::from_str(&serialized).unwrap();

        // Test that reverse mapping works
        assert_eq!(deserialized.get_id("TestPlayer"), Some(id));

        // Test that duplicate detection still works
        let mut names = deserialized;
        let new_id = Id::new();
        let result = names.set_name(new_id, "TestPlayer", Profanity::Censor);
        assert_eq!(result, Err(Error::Used));
    }

    #[test]
    fn test_error_display() {
        assert_eq!(Error::Used.to_string(), "name already in-use");
        assert_eq!(Error::Assigned.to_string(), "player has an existing name");
        assert_eq!(Error::Empty.to_string(), "name cannot be empty");
        assert_eq!(Error::Sinful.to_string(), "name is inappropriate");
        assert_eq!(Error::TooLong.to_string(), "name is too long");
    }

    #[test]
    fn test_names_case_sensitivity() {
        let mut names = Names::default();
        let id1 = Id::new();
        let id2 = Id::new();

        names.set_name(id1, "Player", Profanity::Censor).unwrap();

        // Different case should be allowed
        let result = names.set_name(id2, "player", Profanity::Censor);
        assert!(result.is_ok());

        assert_eq!(names.get_id("Player"), Some(id1));
        assert_eq!(names.get_id("player"), Some(id2));
    }

    #[test]
    fn test_names_unicode_support() {
        let mut names = Names::default();
        let id = Id::new();

        let unicode_name = "Плеер测试🎮";
        let result = names.set_name(id, unicode_name, Profanity::Censor);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), unicode_name);

        assert_eq!(names.get_name(&id), Some(unicode_name));
        assert_eq!(names.get_id(unicode_name), Some(id));
    }

    #[test]
    fn test_name_style_default() {
        let default_style = NameStyle::default();
        match default_style {
            NameStyle::Petname(count) => assert_eq!(count, 2),
            NameStyle::Roman(_) => panic!("Default should be Petname(2)"),
        }
    }

    #[test]
    fn test_name_style_roman_name_generation() {
        // Test Roman style with 2 words (no praenomen)
        let style_2 = NameStyle::Roman(2);
        let name_2 = style_2.get_name();
        assert!(!name_2.is_empty());
        assert!(name_2.chars().next().unwrap().is_uppercase());

        // Test Roman style with 3 words (with praenomen)
        let style_3 = NameStyle::Roman(3);
        let name_3 = style_3.get_name();
        assert!(!name_3.is_empty());
        assert!(name_3.chars().next().unwrap().is_uppercase());
    }

    #[test]
    fn test_name_style_petname_generation() {
        // Test Petname style with 2 words
        let style_2 = NameStyle::Petname(2);
        let name_2 = style_2.get_name();
        assert!(!name_2.is_empty());
        // Should be title case after processing
        assert!(name_2.contains(' ')); // Should have multiple words

        // Test Petname style with 3 words
        let style_3 = NameStyle::Petname(3);
        let name_3 = style_3.get_name();
        assert!(!name_3.is_empty());
        // Count spaces to verify word count (3 words = 2 spaces)
        assert_eq!(name_3.matches(' ').count(), 2);
    }

    #[test]
    fn test_naming_scheme_implementation() {
        let roman_style = NameStyle::Roman(2);
        let petname_style = NameStyle::Petname(2);

        // Test that NamingScheme trait works
        let roman_name = NamingScheme::get_name(&roman_style);
        let petname_name = NamingScheme::get_name(&petname_style);

        assert!(!roman_name.is_empty());
        assert!(!petname_name.is_empty());

        // Test plural name generation
        let roman_plural = roman_style.get_plural_name();
        let petname_plural = petname_style.get_plural_name();

        assert!(!roman_plural.is_empty());
        assert!(!petname_plural.is_empty());
    }
}
