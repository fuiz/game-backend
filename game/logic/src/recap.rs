//! Each player's record of the slides they have played
//!
//! A slide's answers live in its runtime state, which is dropped when the game
//! moves on. The recap keeps what a player needs to look back at a finished
//! slide: what was asked, what they answered, and what was right.

use rustc_hash::FxHashMap;
#[cfg(feature = "serializable")]
use serde::Deserialize;
use serde::Serialize;

use crate::{time::Timestamp, watcher::Id};

/// How one answer fared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "serializable", derive(Deserialize))]
pub enum Verdict {
    /// Fully right.
    Correct,
    /// Earned some of the points but not all, like a multiple-answer question
    /// with one choice missed.
    Partial,
    /// Earned nothing.
    Incorrect,
    /// The player gave no answer.
    Unanswered,
    /// An answer to an opinion slide: there is nothing to be right about.
    Recorded,
}

/// What one player answered on one slide.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct GivenAnswer {
    /// The answer as display text, absent when words can't carry it, like a
    /// spot on a picture.
    pub(crate) answer: Option<String>,
    /// When it was given. In a team game the earliest answer is the one the
    /// team scores on, so the recap shows that one.
    pub(crate) answered_at: Timestamp,
    /// Whether it was right. Never [`Verdict::Unanswered`].
    pub(crate) verdict: Verdict,
}

/// A finished slide as every player saw it.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct SlideRecap {
    /// The slide's position in the quiz.
    pub(crate) index: usize,
    /// The question's number as players count them: info slides ask nothing,
    /// so they take no number. Starts at 1.
    #[cfg_attr(feature = "serializable", serde(default))]
    pub(crate) number: usize,
    /// What was asked.
    pub(crate) question: String,
    /// The right answer as display text, when the slide has one that words can
    /// name. A pin target has none.
    pub(crate) correct_answer: Option<String>,
    /// Whether answers on this slide are right or wrong at all.
    #[cfg_attr(feature = "serializable", serde(default))]
    pub(crate) graded: bool,
    /// Every answer on file, by the player who gave it.
    pub(crate) answers: FxHashMap<Id, GivenAnswer>,
}

/// One slide of a player's recap, as sent to that player.
#[derive(Debug, Serialize, Clone)]
pub struct RecapEntry<'a> {
    /// The slide's position in the quiz.
    pub index: usize,
    /// The question's number as players count them, from 1.
    pub number: usize,
    /// What was asked.
    pub question: &'a str,
    /// What the player answered, absent when they did not.
    pub answer: Option<&'a str>,
    /// The right answer, absent when the slide has none to name.
    pub correct_answer: Option<&'a str>,
    /// Whether the slide has a right answer, so a player who skipped a poll
    /// is not counted as missing a question.
    pub graded: bool,
    /// How the player's answer fared.
    pub verdict: Verdict,
    /// Points earned on the slide, absent when the game hides scores. In a team
    /// game these are the team's.
    pub points: Option<u64>,
}

impl SlideRecap {
    /// This slide from `player`'s side.
    pub(crate) fn entry_for(&self, player: Id, points: Option<u64>) -> RecapEntry<'_> {
        let given = self.answers.get(&player);
        RecapEntry {
            index: self.index,
            number: self.number,
            question: &self.question,
            answer: given.and_then(|given| given.answer.as_deref()),
            correct_answer: self.correct_answer.as_deref(),
            graded: self.graded,
            verdict: given.map_or(Verdict::Unanswered, |given| given.verdict),
            points,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn recap(correct_answer: Option<&str>) -> SlideRecap {
        SlideRecap {
            index: 3,
            number: 2,
            question: "Largest planet?".to_owned(),
            correct_answer: correct_answer.map(str::to_owned),
            graded: correct_answer.is_some(),
            answers: FxHashMap::default(),
        }
    }

    #[test]
    fn a_given_answer_keeps_its_verdict() {
        let player = Id::new();
        let mut recap = recap(Some("Jupiter"));
        recap.answers.insert(
            player,
            GivenAnswer {
                answer: Some("Saturn".to_owned()),
                answered_at: Timestamp::now(),
                verdict: Verdict::Incorrect,
            },
        );

        let entry = recap.entry_for(player, Some(0));
        assert_eq!(entry.index, 3);
        assert_eq!(entry.number, 2);
        assert_eq!(entry.answer, Some("Saturn"));
        assert_eq!(entry.correct_answer, Some("Jupiter"));
        assert_eq!(entry.verdict, Verdict::Incorrect);
    }

    #[test]
    fn no_answer_is_unanswered() {
        let recap = recap(Some("Jupiter"));
        let entry = recap.entry_for(Id::new(), Some(0));
        assert_eq!(entry.answer, None);
        assert_eq!(entry.verdict, Verdict::Unanswered);
    }

    #[test]
    fn no_answer_to_an_opinion_slide_is_still_unanswered() {
        let recap = recap(None);
        let entry = recap.entry_for(Id::new(), None);
        assert_eq!(entry.verdict, Verdict::Unanswered);
        assert_eq!(entry.correct_answer, None);
        assert!(!entry.graded);
    }
}
