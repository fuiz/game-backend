//! Runtime configuration for the Fuiz game system.
//!
//! [`Settings`] replaces compile-time constants with runtime-configurable values.
//! It serves as the garde validation context and optionally integrates with
//! [figment](https://docs.rs/figment) for flexible configuration sourcing.
//!
//! # Library Pattern (figment)
//!
//! `Settings` implements `Default` with sensible defaults. When the `figment`
//! feature is enabled, it also implements [`figment::Provider`], allowing
//! downstream applications to compose it with other configuration sources:
//!
//! ```ignore
//! use figment::{Figment, providers::{Toml, Env}};
//! use fuiz::settings::Settings;
//!
//! let settings: Settings = Figment::new()
//!     .merge(Settings::default())
//!     .merge(Toml::file("Fuiz.toml"))
//!     .merge(Env::prefixed("FUIZ_").split("__"))
//!     .extract()
//!     .unwrap();
//! ```

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Top-level configuration for the Fuiz game system.
///
/// Used as the garde validation context via `#[garde(context(Settings as ctx))]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Fuiz-level limits (slides, title, players).
    pub fuiz: FuizSettings,
    /// Limits shared across all question types.
    pub question: QuestionSettings,
    /// Multiple-choice-specific limits.
    pub multiple_choice: MultipleChoiceSettings,
    /// Type-answer-specific limits.
    pub type_answer: TypeAnswerSettings,
    /// Order-question-specific limits.
    pub order: OrderSettings,
    /// Corkboard (media) limits.
    pub corkboard: CorkboardSettings,
    /// Answer text limits.
    pub answer_text: AnswerTextSettings,
}

/// Limits for the overall Fuiz game.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FuizSettings {
    /// Maximum number of slides allowed in a single Fuiz game.
    pub max_slides_count: usize,
    /// Maximum length of a Fuiz title in characters.
    pub max_title_length: usize,
    /// Maximum number of players allowed in a single game session.
    pub max_player_count: usize,
}

/// Limits shared across all question types (title, timing).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionSettings {
    /// Minimum length of a question title.
    pub min_title_length: usize,
    /// Maximum length of a question title.
    pub max_title_length: usize,
    /// Minimum time limit in seconds for answering a question.
    pub min_time_limit: u64,
    /// Maximum time limit in seconds for answering a question.
    pub max_time_limit: u64,
    /// Minimum time in seconds to introduce/display a question.
    pub min_introduce_question: u64,
    /// Maximum time in seconds to introduce/display a question.
    pub max_introduce_question: u64,
    /// Minimum duration in seconds of the slide-announcement intro.
    pub min_introduce_slide: u64,
    /// Maximum duration in seconds of the slide-announcement intro.
    pub max_introduce_slide: u64,
}

/// Multiple-choice-specific limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultipleChoiceSettings {
    /// Maximum number of answer options.
    pub max_answer_count: usize,
}

/// Type-answer-specific limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TypeAnswerSettings {
    /// Maximum number of acceptable answers.
    pub max_answer_count: usize,
}

/// Order-question-specific limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderSettings {
    /// Maximum number of items to order.
    pub max_answer_count: usize,
    /// Maximum length of an axis label.
    pub max_label_length: usize,
}

/// Corkboard (media storage) limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorkboardSettings {
    /// Length of generated IDs for corkboard items.
    pub id_length: usize,
    /// Maximum length of alt text for accessibility.
    pub max_alt_length: usize,
}

/// Answer text limits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerTextSettings {
    /// Maximum length of answer text in characters.
    pub max_length: usize,
}

// --- Defaults (previously compile-time constants) ---

impl Default for FuizSettings {
    fn default() -> Self {
        Self {
            max_slides_count: 500,
            max_title_length: 500,
            max_player_count: 1000,
        }
    }
}

impl Default for QuestionSettings {
    fn default() -> Self {
        Self {
            min_title_length: 0,
            max_title_length: 500,
            min_time_limit: 5,
            max_time_limit: 240,
            min_introduce_question: 0,
            max_introduce_question: 240,
            min_introduce_slide: 0,
            max_introduce_slide: 240,
        }
    }
}

impl Default for MultipleChoiceSettings {
    fn default() -> Self {
        Self { max_answer_count: 8 }
    }
}

impl Default for TypeAnswerSettings {
    fn default() -> Self {
        Self { max_answer_count: 16 }
    }
}

impl Default for OrderSettings {
    fn default() -> Self {
        Self {
            max_answer_count: 8,
            max_label_length: 250,
        }
    }
}

impl Default for CorkboardSettings {
    fn default() -> Self {
        Self {
            id_length: 16,
            max_alt_length: 200,
        }
    }
}

impl Default for AnswerTextSettings {
    fn default() -> Self {
        Self { max_length: 500 }
    }
}

// --- Duration validation helpers ---

impl QuestionSettings {
    /// Validate that a duration is within the time limit bounds.
    ///
    /// # Errors
    ///
    /// Returns a [`garde::Error`] if the duration is outside the configured bounds.
    pub fn validate_time_limit(&self, val: &Option<Duration>) -> garde::Result {
        match val {
            None => Ok(()),
            Some(duration) => validate_duration_range(duration, self.min_time_limit, self.max_time_limit),
        }
    }

    /// Validate that a duration is within the introduce-question bounds.
    ///
    /// # Errors
    ///
    /// Returns a [`garde::Error`] if the duration is outside the configured bounds.
    pub fn validate_introduce_question(&self, val: &Option<Duration>) -> garde::Result {
        match val {
            None => Ok(()),
            Some(duration) => {
                validate_duration_range(duration, self.min_introduce_question, self.max_introduce_question)
            }
        }
    }

    /// Validate that a slide-announcement duration is within bounds. `None`
    /// (host-paced, no auto-advance) is always valid.
    ///
    /// # Errors
    ///
    /// Returns a [`garde::Error`] if the duration is outside the configured bounds.
    pub fn validate_introduce_slide(&self, val: &Option<Duration>) -> garde::Result {
        match val {
            None => Ok(()),
            Some(duration) => validate_duration_range(duration, self.min_introduce_slide, self.max_introduce_slide),
        }
    }
}

fn validate_duration_range(val: &Duration, min: u64, max: u64) -> garde::Result {
    if (min..=max).contains(&val.as_secs()) {
        Ok(())
    } else {
        Err(garde::Error::new(format!("outside of bounds [{min},{max}]")))
    }
}

// --- Figment Provider (behind feature flag) ---

#[cfg(feature = "figment")]
impl figment::Provider for Settings {
    fn metadata(&self) -> figment::Metadata {
        figment::Metadata::named("Fuiz Game Settings")
    }

    fn data(&self) -> Result<figment::value::Map<figment::Profile, figment::value::Dict>, figment::Error> {
        figment::providers::Serialized::defaults(self).data()
    }
}

#[cfg(feature = "figment")]
impl Settings {
    /// Extract settings from any figment [`Provider`](figment::Provider).
    ///
    /// # Errors
    ///
    /// Returns a figment error if extraction fails.
    #[allow(clippy::result_large_err)]
    pub fn from<T: figment::Provider>(provider: T) -> Result<Self, figment::Error> {
        figment::Figment::from(provider).extract()
    }

    /// A default [`Figment`](figment::Figment) seeded with `Settings::default()`.
    ///
    /// Downstream applications can merge additional sources on top:
    /// ```ignore
    /// let figment = Settings::figment()
    ///     .merge(Toml::file("Fuiz.toml"))
    ///     .merge(Env::prefixed("FUIZ_").split("__"));
    /// ```
    pub fn figment() -> figment::Figment {
        figment::Figment::from(Self::default())
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn validate_introduce_slide_bounds() {
        let question = QuestionSettings::default();

        // Host-paced (no auto-advance) is always valid.
        assert!(question.validate_introduce_slide(&None).is_ok());
        // Within the default [0, 240] bounds.
        assert!(question.validate_introduce_slide(&Some(Duration::ZERO)).is_ok());
        assert!(question.validate_introduce_slide(&Some(Duration::from_secs(3))).is_ok());
        assert!(
            question
                .validate_introduce_slide(&Some(Duration::from_secs(question.max_introduce_slide)))
                .is_ok()
        );
        // Beyond the maximum is rejected.
        assert!(
            question
                .validate_introduce_slide(&Some(Duration::from_secs(question.max_introduce_slide + 1)))
                .is_err()
        );
    }
}
