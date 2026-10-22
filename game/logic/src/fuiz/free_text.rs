//! Free-text (word cloud and open ended) question implementation
//!
//! Players type their own answers instead of picking from a list. Two flavours
//! share this module because both collect a bag of strings and report how often
//! each was said; only the presentation and the normalisation differ:
//!
//! - [`Mode::WordCloud`]: short entries, several per player, matched
//!   case-insensitively so "Paris" and "paris" pile into the same word.
//! - [`Mode::OpenEnded`]: one longer response per player, kept verbatim.
//!
//! Both collect opinions, so no answer is correct and no points are awarded.

use std::time::Duration;

use garde::Validate;
use itertools::Itertools;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_with::DurationMilliSeconds;

use crate::tick::Tick;
use crate::time::Timestamp;
use crate::{
    fuiz::config::{ScheduleMessageFn, SlideAction},
    leaderboard::Leaderboard,
    session::TunnelFinder,
    teams::TeamManager,
    watcher::{Id, ValueKind, Watchers},
};

use super::{
    super::game::IncomingPlayerMessage,
    common::{
        AnswerHandler, PhasedSlide, ProceedFromSlideIntoSlide, QuestionReceiveMessage, SlideCore, SlideStateManager,
        SlideTimer, get_answered_count, impl_slide_core,
    },
    media::Media,
};

/// Lifecycle phases for a free-text slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without the input.
    Question,
    /// Accepting entries from players.
    Answers,
    /// Displaying the collected entries.
    AnswersResults,
}

impl super::common::Phase for Phase {
    fn next(self) -> Option<Self> {
        match self {
            Self::Unstarted => Some(Self::Question),
            Self::Question => Some(Self::Answers),
            Self::Answers => Some(Self::AnswersResults),
            Self::AnswersResults => None,
        }
    }
}

/// Which flavour of free-text collection this slide runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    /// Short entries piled into a word cloud, matched case-insensitively.
    #[default]
    WordCloud,
    /// Longer responses listed verbatim.
    OpenEnded,
}

/// Configuration for a free-text slide
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The question title, represents what's being asked
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Accompanying media
    #[garde(dive)]
    media: Option<Media>,
    /// Duration of the slide-announcement intro shown before the question.
    /// Absent means a default duration; `null` means host-paced.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_slide(val)))]
    #[serde(
        default = "crate::fuiz::common::default_introduce_slide",
        with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>"
    )]
    introduce_slide: Option<Duration>,
    /// Time before the input is displayed.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can submit entries.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Points awarded. Free-text slides collect opinions, so this is `0` in
    /// practice; the field exists so every slide type announces the same way.
    #[garde(skip)]
    #[serde(default)]
    points_awarded: u64,
    /// Which flavour of collection to run
    #[garde(skip)]
    #[serde(default)]
    mode: Mode,
    /// How many entries a single player may submit
    #[garde(range(min = 1, max = ctx.free_text.max_entries_per_player))]
    #[serde(default = "default_max_entries")]
    max_entries: usize,
    /// Maximum length of a single entry in characters
    #[garde(range(min = 1, max = ctx.free_text.max_entry_length))]
    #[serde(default = "default_max_entry_length")]
    max_entry_length: usize,
}

fn default_max_entries() -> usize {
    1
}

fn default_max_entry_length() -> usize {
    crate::settings::FreeTextSettings::default().max_entry_length
}

/// Runtime state for a free-text slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Each player's entries (already normalised) with submission timestamps
    user_answers: FxHashMap<Id, (Vec<String>, Timestamp)>,
    /// Shared runtime core: slide phase, answer-start timestamp, live-answered tally.
    #[cfg_attr(feature = "serializable", serde(flatten))]
    core: SlideCore<Phase>,
}

impl SlideConfig {
    /// Creates a new runtime state from this configuration
    pub fn to_state(&self) -> State {
        State {
            config: self.clone(),
            user_answers: FxHashMap::default(),
            core: SlideCore::default(),
        }
    }
}

/// One collected entry and how many players said it.
#[derive(Debug, Clone, Serialize)]
pub struct EntryCount {
    /// The entry as it will be displayed.
    pub text: String,
    /// How many players submitted it.
    pub count: usize,
}

/// Aggregated results for a free-text slide.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    /// Distinct entries, most frequent first.
    pub entries: Vec<EntryCount>,
    /// Total number of entries submitted, before deduplication.
    pub total_entries: usize,
    /// Number of players who submitted at least one entry.
    pub total_count: usize,
}

/// Messages sent to listeners to update their pre-existing free-text state.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Announces the upcoming question's type and scoring (the `Unstarted` phase).
    SlideAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// Points awarded (zero for opinion slides)
        points_awarded: u64,
        /// Duration of the intro, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Which flavour of collection is coming up
        mode: Mode,
    },
    /// Announces the question without the input
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Time before the input appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Opens the submission window
    AnswersAnnouncement {
        /// Time before submission closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Which flavour of collection this is
        mode: Mode,
        /// How many entries a single player may submit
        max_entries: usize,
        /// Maximum length of a single entry
        max_entry_length: usize,
    },
    /// (HOST ONLY) Reports the number of players who have submitted
    AnswersCount(usize),
    /// Shows the collected entries
    AnswersResults {
        /// Which flavour of collection this was
        mode: Mode,
        /// Every distinct entry, for the host's screen. Absent for everyone
        /// else, who are shown only that their own answer was recorded.
        results: Option<Results>,
    },
}

/// Scheduled phase-transition alarm for free-text slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing free-text state.
///
/// See [`UpdateMessage`] for an explanation of these fields.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage<'a> {
    /// Synchronizes the slide-announcement intro phase (`Unstarted`)
    SlideAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// Points awarded (zero for opinion slides)
        points_awarded: u64,
        /// Remaining intro time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Which flavour of collection is coming up
        mode: Mode,
    },
    /// Synchronizes the question announcement phase
    QuestionAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Remaining time before the input appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Synchronizes the submission phase
    AnswersAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Remaining submission time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Which flavour of collection this is
        mode: Mode,
        /// How many entries a single player may submit
        max_entries: usize,
        /// Maximum length of a single entry
        max_entry_length: usize,
        /// Number of players who have already submitted
        answered_count: usize,
    },
    /// Synchronizes the results phase
    AnswersResults {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text that was asked
        question: &'a str,
        /// Optional media content that accompanied the question
        media: Option<&'a Media>,
        /// Which flavour of collection this was
        mode: Mode,
        /// Every distinct entry, for the host's screen. Absent for everyone
        /// else, who are shown only that their own answer was recorded.
        results: Option<Results>,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<Vec<String>> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (Vec<String>, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (Vec<String>, Timestamp)> {
        &mut self.user_answers
    }

    fn transform_answer(&self, answer: Vec<String>) -> Vec<String> {
        answer
            .into_iter()
            .map(|entry| self.config.normalize(&entry))
            .filter(|entry| !entry.is_empty())
            .unique()
            .take(self.config.max_entries)
            .collect_vec()
    }

    /// Opinions have no right answer, so nothing scores.
    fn is_correct_answer(&self, _answer: &Vec<String>) -> bool {
        false
    }

    fn describe_answer(&self, answer: &Vec<String>) -> String {
        answer.join(", ")
    }

    fn question_text(&self) -> &str {
        &self.config.title
    }

    fn max_points(&self) -> u64 {
        self.config.points_awarded
    }

    fn time_limit(&self) -> Option<Duration> {
        self.config.time_limit
    }

    fn answers_count_message(count: usize) -> crate::UpdateMessage<'static> {
        UpdateMessage::AnswersCount(count).into()
    }

    fn send_answers_results<F: TunnelFinder>(&mut self, watchers: &Watchers, tunnel_finder: F) {
        if self.change_state(Phase::Answers, Phase::AnswersResults) {
            let results = self.results();
            watchers.announce_with(
                |_, kind| {
                    Some(
                        UpdateMessage::AnswersResults {
                            mode: self.config.mode,
                            results: matches!(kind, ValueKind::Host).then(|| results.clone()),
                        }
                        .into(),
                    )
                },
                tunnel_finder,
            );
        }
    }
}

impl SlideConfig {
    /// Trims an entry, truncates it to the configured length, and, for word
    /// clouds, lowercases it so casing variants pile into one word.
    fn normalize(&self, entry: &str) -> String {
        let trimmed = entry.trim();
        let truncated: String = trimmed.chars().take(self.max_entry_length).collect();
        match self.mode {
            Mode::WordCloud => truncated.to_lowercase(),
            Mode::OpenEnded => truncated,
        }
    }
}

impl PhasedSlide<Vec<String>> for State {
    fn enter_phase<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        phase: Phase,
        _team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        match phase {
            Phase::Unstarted => {
                self.announce_slide(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Phase::Question => {
                if !self.change_state(Phase::Unstarted, Phase::Question) {
                    return;
                }

                self.start_timer_at(tick.now());

                // Announced before any skip ahead. This is the only live
                // message carrying the question and its media, and the one
                // that opens the input carries neither, so a zero-length
                // intro must not be allowed to swallow it.
                watchers.announce(
                    &UpdateMessage::QuestionAnnouncement {
                        index,
                        count,
                        question: &self.config.title,
                        media: self.config.media.as_ref(),
                        duration: self.config.introduce_question,
                    }
                    .into(),
                    &tunnel_finder,
                );

                if let Some(duration) = self.config.introduce_question {
                    if duration.is_zero() {
                        self.enter_phase(
                            Phase::Answers,
                            None,
                            watchers,
                            schedule_message,
                            tick,
                            tunnel_finder,
                            index,
                            count,
                        );
                    } else {
                        schedule_message(
                            AlarmMessage {
                                index,
                                to: Phase::Answers,
                            }
                            .into(),
                            duration,
                        );
                    }
                }
            }
            Phase::Answers => {
                if !self.change_state(Phase::Question, Phase::Answers) {
                    return;
                }
                self.start_timer_at(tick.now());
                self.reserve_for_players(watchers.specific_count(ValueKind::Player));

                watchers.announce(
                    &UpdateMessage::AnswersAnnouncement {
                        duration: self.config.time_limit,
                        mode: self.config.mode,
                        max_entries: self.config.max_entries,
                        max_entry_length: self.config.max_entry_length,
                    }
                    .into(),
                    tunnel_finder,
                );

                if let Some(time_limit) = self.config.time_limit {
                    schedule_message(
                        AlarmMessage {
                            index,
                            to: Phase::AnswersResults,
                        }
                        .into(),
                        time_limit,
                    );
                }
            }
            Phase::AnswersResults => {
                self.send_answers_results(watchers, tunnel_finder);
            }
        }
    }
}

impl State {
    /// Announces the upcoming question's type (the `Unstarted` phase), then
    /// auto-advances after `introduce_slide`.
    fn announce_slide<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        watchers.announce(
            &UpdateMessage::SlideAnnouncement {
                index,
                count,
                points_awarded: self.config.points_awarded,
                duration: self.config.introduce_slide,
                mode: self.config.mode,
            }
            .into(),
            &tunnel_finder,
        );
        match self.config.introduce_slide {
            Some(duration) if duration.is_zero() => self.enter_phase(
                Phase::Question,
                None,
                watchers,
                schedule_message,
                tick,
                tunnel_finder,
                index,
                count,
            ),
            Some(duration) => schedule_message(
                AlarmMessage {
                    index,
                    to: Phase::Question,
                }
                .into(),
                duration,
            ),
            None => {}
        }
    }

    /// Counts how often each distinct entry was submitted, most frequent first.
    ///
    /// Ties break alphabetically so the cloud doesn't reshuffle between the
    /// live results and a later reconnect's sync message.
    fn results(&self) -> Results {
        let mut counts: FxHashMap<&str, usize> = FxHashMap::default();
        let mut total_entries = 0;
        for (entries, _) in self.user_answers.values() {
            for entry in entries {
                *counts.entry(entry.as_str()).or_default() += 1;
                total_entries += 1;
            }
        }

        Results {
            entries: counts
                .into_iter()
                .sorted_by(|(a_text, a_count), (b_text, b_count)| b_count.cmp(a_count).then_with(|| a_text.cmp(b_text)))
                .map(|(text, count)| EntryCount {
                    text: text.to_string(),
                    count,
                })
                .collect_vec(),
            total_entries,
            total_count: self.user_answers.len(),
        }
    }

    /// Starts the free-text slide by entering the [`Phase::Unstarted`] phase.
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        self.enter_phase(
            Phase::Unstarted,
            None,
            watchers,
            schedule_message,
            tick,
            tunnel_finder,
            index,
            count,
        );
    }

    /// Synchronization message for a newly connected watcher.
    pub fn state_message(&self, watcher_kind: ValueKind, index: usize, count: usize) -> SyncMessage<'_> {
        match self.state() {
            Phase::Unstarted => SyncMessage::SlideAnnouncement {
                index,
                count,
                points_awarded: self.config.points_awarded,
                duration: self.config.introduce_slide,
                mode: self.config.mode,
            },
            Phase::Question => SyncMessage::QuestionAnnouncement {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                duration: self
                    .config
                    .introduce_question
                    .map(|duration| duration.saturating_sub(self.elapsed())),
            },
            Phase::Answers => SyncMessage::AnswersAnnouncement {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                duration: self
                    .config
                    .time_limit
                    .map(|duration| duration.saturating_sub(self.elapsed())),
                mode: self.config.mode,
                max_entries: self.config.max_entries,
                max_entry_length: self.config.max_entry_length,
                answered_count: get_answered_count(self),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                mode: self.config.mode,
                results: matches!(watcher_kind, ValueKind::Host).then(|| self.results()),
            },
        }
    }

    /// Forwards a phase-transition alarm to [`PhasedSlide::default_receive_alarm`].
    pub(crate) fn receive_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        message: &crate::AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        if let crate::AlarmMessage::FreeText(inner) = message {
            self.default_receive_alarm(
                inner.to,
                None,
                watchers,
                schedule_message,
                tick,
                tunnel_finder,
                index,
                count,
            )
        } else {
            SlideAction::Stay
        }
    }
}

impl QuestionReceiveMessage for State {
    fn receive_host_next<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        self.default_receive_host_next(
            leaderboard,
            watchers,
            team_manager,
            schedule_message,
            tick,
            tunnel_finder,
            index,
            count,
        )
    }

    fn receive_player_message<F: TunnelFinder>(
        &mut self,
        watcher_id: Id,
        message: IncomingPlayerMessage,
        watchers: &Watchers,
        tick: Tick,
        tunnel_finder: F,
    ) {
        if self.state() != Phase::Answers {
            return;
        }
        // A single-entry slide can send either shape; both land in the same bag.
        let entries = match message {
            IncomingPlayerMessage::StringArrayAnswer(entries) => entries,
            IncomingPlayerMessage::StringAnswer(entry) => vec![entry],
            _ => return,
        };
        // An all-blank submission would otherwise mark the player as answered
        // while contributing nothing.
        if self.transform_answer(entries.clone()).is_empty() {
            return;
        }
        self.record_answer_at(watcher_id, entries, tick.now());
        self.handle_post_answer(watchers, &tunnel_finder);
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn config(mode: Mode, max_entries: usize) -> SlideConfig {
        SlideConfig {
            title: "One word for today?".to_string(),
            media: None,
            introduce_slide: None,
            introduce_question: None,
            time_limit: None,
            points_awarded: 0,
            mode,
            max_entries,
            max_entry_length: 200,
        }
    }

    #[test]
    fn word_clouds_fold_case_and_whitespace_together() {
        let mut state = config(Mode::WordCloud, 3).to_state();
        state.record_answer(Id::new(), vec!["Paris".to_string()]);
        state.record_answer(Id::new(), vec!["  paris ".to_string()]);
        state.record_answer(Id::new(), vec!["Rome".to_string()]);

        let results = state.results();
        assert_eq!(
            results
                .entries
                .iter()
                .map(|e| (e.text.as_str(), e.count))
                .collect::<Vec<_>>(),
            vec![("paris", 2), ("rome", 1)]
        );
        assert_eq!(results.total_entries, 3);
        assert_eq!(results.total_count, 3);
    }

    #[test]
    fn open_ended_keeps_casing() {
        let mut state = config(Mode::OpenEnded, 1).to_state();
        state.record_answer(Id::new(), vec!["  It Was Great  ".to_string()]);
        assert_eq!(state.results().entries[0].text, "It Was Great");
    }

    #[test]
    fn a_players_duplicate_entries_count_once() {
        let mut state = config(Mode::WordCloud, 3).to_state();
        state.record_answer(Id::new(), vec!["blue".to_string(), "Blue".to_string()]);
        let results = state.results();
        assert_eq!(results.entries.len(), 1);
        assert_eq!(results.entries[0].count, 1);
    }

    #[test]
    fn entries_beyond_the_cap_are_dropped() {
        let mut state = config(Mode::WordCloud, 2).to_state();
        state.record_answer(
            Id::new(),
            vec!["one".to_string(), "two".to_string(), "three".to_string()],
        );
        assert_eq!(state.results().total_entries, 2);
    }

    #[test]
    fn blank_entries_are_discarded() {
        let mut state = config(Mode::WordCloud, 3).to_state();
        state.record_answer(Id::new(), vec!["   ".to_string(), "real".to_string()]);
        let results = state.results();
        assert_eq!(results.entries.len(), 1);
        assert_eq!(results.entries[0].text, "real");
    }

    #[test]
    fn overlong_entries_are_truncated() {
        let mut state = config(Mode::OpenEnded, 1).to_state();
        state.config.max_entry_length = 5;
        state.record_answer(Id::new(), vec!["abcdefghij".to_string()]);
        assert_eq!(state.results().entries[0].text, "abcde");
    }

    #[test]
    fn ties_break_alphabetically_for_a_stable_cloud() {
        let mut state = config(Mode::WordCloud, 1).to_state();
        state.record_answer(Id::new(), vec!["zebra".to_string()]);
        state.record_answer(Id::new(), vec!["apple".to_string()]);
        let texts = state
            .results()
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect::<Vec<_>>();
        assert_eq!(texts, vec!["apple", "zebra"]);
    }

    #[test]
    fn opinions_never_score() {
        let state = config(Mode::WordCloud, 3).to_state();
        assert!(!state.is_correct_answer(&vec!["anything".to_string()]));
    }
}
