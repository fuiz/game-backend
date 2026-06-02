//! Multiple choice question implementation
//!
//! This module implements the multiple choice question type for Fuiz games.
//! Multiple choice questions present a question followed by several answer
//! options, allowing players to select one correct answer. The module handles
//! timing, scoring, answer validation, and result presentation.

use std::time::Duration;

use crate::time::Timestamp;
use garde::Validate;
use itertools::Itertools;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use serde_with::DurationMilliSeconds;

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
        AnswerHandler, HasSlideCore, PhasedSlide, ProceedFromSlideIntoSlide, QuestionReceiveMessage, SlideCore,
        SlideStateManager, SlideTimer, get_answered_count,
    },
    config::{TextOrMedia, TextOrMediaRef},
    media::Media,
};

/// Controls whether a multiple choice question accepts a single answer or multiple answers
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum AnswerMode {
    /// Players pick one answer; full time-based points if correct (default behavior)
    #[default]
    SingleAnswer,
    /// Players pick multiple answers; graduated scoring based on correct/wrong ratio
    MultipleAnswers,
}

/// Lifecycle phases for a multiple-choice slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without answer options.
    Question,
    /// Revealing answer options and accepting player selections.
    Answers,
    /// Displaying results with correct answers and statistics.
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

/// Configuration for a multiple choice question slide
///
/// This struct defines all the parameters needed to create and present
/// a multiple choice question, including timing, content, scoring, and
/// the available answer options.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The question text that will be displayed to players
    #[garde(length(min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Optional media content (images, etc.) to accompany the question
    #[garde(dive)]
    media: Option<Media>,
    /// Duration of the slide-announcement intro shown before the question — an
    /// animation naming the question type and its scoring. Absent → a default
    /// duration; `null` → host-paced (must skip manually); a value → auto-advance
    /// after it. The host can always skip early.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_slide(val)))]
    #[serde(
        default = "crate::fuiz::common::default_introduce_slide",
        with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>"
    )]
    introduce_slide: Option<Duration>,
    /// Duration to display the question before revealing answer options.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Duration players have to select their answer once options are revealed.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Maximum points awarded for a correct answer (decreases linearly over time)
    #[garde(skip)]
    points_awarded: u64,
    /// The available answer choices for this question
    #[garde(length(max = ctx.multiple_choice.max_answer_count))]
    answers: Vec<AnswerChoice>,
    /// Whether the question accepts single or multiple answer selections
    #[garde(skip)]
    #[serde(default)]
    answer_mode: AnswerMode,
}

/// Runtime state for a multiple choice question during gameplay
///
/// This struct maintains the dynamic state of a multiple choice question
/// as it progresses through its phases, tracking player responses,
/// timing information, and current presentation state.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, serde::Deserialize))]
pub struct State {
    /// The configuration this state was created from
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Stores player answers along with the timestamp when they were submitted
    user_answers: FxHashMap<Id, (Vec<usize>, Timestamp)>,
    /// Shared runtime core: slide phase, answer-start timestamp, live-answered tally.
    /// `serde(flatten)` keeps the wire format identical to the pre-refactor layout.
    #[cfg_attr(feature = "serializable", serde(flatten))]
    core: SlideCore<Phase>,
}

impl SlideConfig {
    /// Creates a new runtime state from this configuration
    ///
    /// This method initializes a fresh state for gameplay, setting up
    /// empty answer tracking and the initial unstarted phase.
    ///
    /// # Returns
    ///
    /// A new `State` ready for gameplay
    pub fn to_state(&self) -> State {
        State {
            config: self.clone(),
            user_answers: FxHashMap::default(),
            core: SlideCore::default(),
        }
    }
}

/// Utility type for conditionally hiding content based on viewer permissions
///
/// This enum allows content to be visible to some participants (like hosts)
/// while being hidden from others (like players) until the appropriate time.
#[derive(Debug, Serialize, Clone)]
pub enum PossiblyHidden<T> {
    /// Content is visible to the recipient
    Visible(T),
    /// Content is hidden from the recipient
    Hidden,
}

/// Update messages sent to participants during multiple choice questions
///
/// These messages inform participants about changes in the question state,
/// such as when new phases begin or when results become available.
/// They are sent to participants who already have some context about the slide.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Announces the upcoming question's type and scoring before the question
    /// itself is shown (the `Unstarted` phase). Players see an intro animation.
    SlideAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// Maximum points awarded for a correct answer
        points_awarded: u64,
        /// Duration of the intro before the question is shown, or `None` for
        /// host-paced (the host must advance manually)
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Announces the question without revealing answer options
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Duration before answer options will be revealed, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Announces the answer options for player selection
    AnswersAnnouncement {
        /// Duration before the answering phase ends, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Answer options (may be hidden from some participants)
        answers: Vec<PossiblyHidden<TextOrMediaRef<'a>>>,
        /// Whether the question accepts single or multiple answer selections
        answer_mode: AnswerMode,
    },
    /// (HOST ONLY) Reports the number of players who have submitted answers
    AnswersCount(usize),
    /// Shows the results with correct answers and response statistics
    AnswersResults {
        /// All answer options for the question
        answers: Vec<TextOrMediaRef<'a>>,
        /// Results showing correctness and selection statistics
        results: Vec<AnswerChoiceResult>,
    },
}

/// Scheduled phase-transition alarm for multiple-choice slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Synchronization messages for participants joining during multiple choice questions
///
/// These messages provide complete state information to participants who
/// connect or reconnect during a question, allowing them to synchronize
/// their view with the current state. Similar to UpdateMessage but includes
/// additional context needed for synchronization.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage<'a> {
    /// Synchronizes the slide-announcement intro phase (`Unstarted`)
    SlideAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// Maximum points awarded for a correct answer
        points_awarded: u64,
        /// Duration of the intro before the question is shown, or `None` for
        /// host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
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
        /// Remaining time before answer options will be revealed, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Synchronizes the answer selection phase
    AnswersAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Remaining time before the answering phase ends, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Answer options (may be hidden from some participants)
        answers: Vec<PossiblyHidden<TextOrMediaRef<'a>>>,
        /// Number of players who have already answered
        answered_count: usize,
        /// Whether the question accepts single or multiple answer selections
        answer_mode: AnswerMode,
    },
    /// Results of the game including correct answers and statistics of how many they got chosen
    AnswersResults {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text that was asked
        question: &'a str,
        /// Optional media content that accompanied the question
        media: Option<&'a Media>,
        /// All answer options for the question
        answers: Vec<TextOrMediaRef<'a>>,
        /// Results showing correctness and selection statistics
        results: Vec<AnswerChoiceResult>,
    },
}

/// Represents a single answer option in a multiple choice question
///
/// Each answer choice contains the content to display and whether
/// it is a correct answer to the question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnswerChoice {
    /// Whether this answer choice is correct
    pub correct: bool,
    /// The content of this answer choice (text or media)
    pub content: TextOrMedia,
}

/// Contains correctness information and statistics for an answer choice
///
/// This struct is used in results display to show whether each answer
/// option was correct and how many players selected it.
#[derive(Debug, Serialize, Clone)]
pub struct AnswerChoiceResult {
    /// Whether this answer choice was correct
    correct: bool,
    /// Number of players who selected this answer choice
    count: usize,
}

impl HasSlideCore for State {
    type Phase = Phase;

    fn slide_core(&self) -> &SlideCore<Phase> {
        &self.core
    }

    fn slide_core_mut(&mut self) -> &mut SlideCore<Phase> {
        &mut self.core
    }
}

impl AnswerHandler<Vec<usize>> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (Vec<usize>, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (Vec<usize>, Timestamp)> {
        &mut self.user_answers
    }

    fn is_correct_answer(&self, answer: &Vec<usize>) -> bool {
        match self.config.answer_mode {
            AnswerMode::SingleAnswer => match answer.as_slice() {
                [single] => self.config.answers.get(*single).is_some_and(|x| x.correct),
                _ => false,
            },
            AnswerMode::MultipleAnswers => self.compute_score_multiplier(answer) > 0.0,
        }
    }

    fn score_multiplier(&self, answer: &Vec<usize>) -> f64 {
        match self.config.answer_mode {
            AnswerMode::SingleAnswer => {
                if self.is_correct_answer(answer) {
                    1.0
                } else {
                    0.0
                }
            }
            AnswerMode::MultipleAnswers => self.compute_score_multiplier(answer),
        }
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
            let answer_count = self.per_index_answer_counts();
            watchers.announce(
                &UpdateMessage::AnswersResults {
                    answers: self.config.answers.iter().map(|a| a.content.as_ref()).collect_vec(),
                    results: self
                        .config
                        .answers
                        .iter()
                        .enumerate()
                        .map(|(i, a)| AnswerChoiceResult {
                            correct: a.correct,
                            count: *answer_count.get(&i).unwrap_or(&0),
                        })
                        .collect_vec(),
                }
                .into(),
                tunnel_finder,
            );
        }
    }
}

impl PhasedSlide<Vec<usize>> for State {
    fn enter_phase<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        phase: Phase,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        watchers: &Watchers,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        match phase {
            Phase::Unstarted => {
                self.announce_slide(team_manager, watchers, schedule_message, tunnel_finder, index, count);
            }
            Phase::Question => {
                if !self.change_state(Phase::Unstarted, Phase::Question) {
                    return;
                }
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
                            team_manager,
                            watchers,
                            schedule_message,
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
                self.start_timer();
                self.reserve_for_players(watchers.specific_count(ValueKind::Player));

                watchers.announce_with(
                    |id, kind| match kind {
                        ValueKind::Host | ValueKind::Player => Some(
                            UpdateMessage::AnswersAnnouncement {
                                duration: self.config.time_limit,
                                answers: self.get_answers_for_player(
                                    id,
                                    kind,
                                    team_manager.map_or(1, |tm| tm.alive_team_size(id, &tunnel_finder)),
                                    team_manager.map_or(0, |tm| tm.alive_team_index(id, &tunnel_finder)),
                                    team_manager.is_some(),
                                ),
                                answer_mode: self.config.answer_mode,
                            }
                            .into(),
                        ),
                        ValueKind::Unassigned => None,
                    },
                    &tunnel_finder,
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
    /// Announces the upcoming question's type and scoring (the `Unstarted`
    /// phase), then auto-advances to the question after `introduce_slide` —
    /// immediately if zero, never if `None` (host-paced).
    fn announce_slide<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        watchers: &Watchers,
        schedule_message: S,
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
            }
            .into(),
            &tunnel_finder,
        );
        match self.config.introduce_slide {
            Some(duration) if duration.is_zero() => self.enter_phase(
                Phase::Question,
                team_manager,
                watchers,
                schedule_message,
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

    /// Computes the graduated score multiplier for a multi-answer response.
    ///
    /// Formula: `max(0, (correct_picked - wrong_picked) / total_correct)`
    fn compute_score_multiplier(&self, answer: &[usize]) -> f64 {
        let total_correct = self.config.answers.iter().filter(|a| a.correct).count();
        if total_correct == 0 {
            return 0.0;
        }

        let correct_picked = answer
            .iter()
            .filter(|&&i| self.config.answers.get(i).is_some_and(|a| a.correct))
            .count();
        let wrong_picked = answer.len() - correct_picked;

        let numerator = correct_picked as f64 - wrong_picked as f64;
        (numerator / total_correct as f64).max(0.0)
    }

    /// Counts how many times each answer index was selected across all players.
    ///
    /// Unlike `answer_counts()` which groups by exact `Vec<usize>`, this counts
    /// per-index selections for results display.
    fn per_index_answer_counts(&self) -> FxHashMap<usize, usize> {
        let mut counts = FxHashMap::default();
        for (indices, _) in self.user_answers.values() {
            for &i in indices {
                *counts.entry(i).or_default() += 1;
            }
        }
        counts
    }

    /// Starts the multiple-choice slide by entering [`Phase::Question`].
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        watchers: &Watchers,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        self.enter_phase(
            Phase::Unstarted,
            team_manager,
            watchers,
            schedule_message,
            tunnel_finder,
            index,
            count,
        );
    }

    /// Determines which answer options should be visible to a specific participant
    ///
    /// In individual games, players see all answer options. In team games, answer
    /// options are distributed among team members to encourage collaboration.
    /// Hosts see all options in individual mode but none in team mode.
    ///
    /// # Arguments
    ///
    /// * `_id` - The participant's ID (currently unused)
    /// * `watcher_kind` - The type of participant (host, player, unassigned)
    /// * `team_size` - Number of active members in the participant's team
    /// * `team_index` - The participant's index within their team
    /// * `is_team` - Whether this is a team-based game
    ///
    /// # Returns
    ///
    /// A vector of answer options, some potentially hidden based on game mode and participant role
    fn get_answers_for_player(
        &self,
        _id: Id,
        watcher_kind: ValueKind,
        team_size: usize,
        team_index: usize,
        is_team: bool,
    ) -> Vec<PossiblyHidden<TextOrMediaRef<'_>>> {
        match watcher_kind {
            ValueKind::Host | ValueKind::Unassigned => {
                if is_team {
                    std::iter::repeat_n(PossiblyHidden::Hidden, self.config.answers.len()).collect_vec()
                } else {
                    self.config
                        .answers
                        .iter()
                        .map(|answer_choice| PossiblyHidden::Visible(answer_choice.content.as_ref()))
                        .collect_vec()
                }
            }
            ValueKind::Player => match self.config.answers.len() {
                0 => Vec::new(),
                answer_count => {
                    let adjusted_team_index = (team_index % team_size) % answer_count;

                    self.config
                        .answers
                        .iter()
                        .enumerate()
                        .map(|(answer_index, answer_choice)| {
                            if answer_index % team_size == adjusted_team_index {
                                PossiblyHidden::Visible(answer_choice.content.as_ref())
                            } else {
                                PossiblyHidden::Hidden
                            }
                        })
                        .collect_vec()
                }
            },
        }
    }

    /// Generates a synchronization message for a participant joining during the question
    ///
    /// This method creates the appropriate sync message based on the current slide state,
    /// allowing newly connected participants to see the current question state with
    /// correct timing and answer visibility.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the participant to synchronize
    /// * `watcher_kind` - Type of participant (host, player, unassigned)
    /// * `team_manager` - Optional team manager for team-based games
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
    ///
    /// # Returns
    ///
    /// A `SyncMessage` appropriate for the current state and participant type
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    pub fn state_message<F: TunnelFinder>(
        &self,
        watcher_id: Id,
        watcher_kind: ValueKind,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SyncMessage<'_> {
        match self.state() {
            Phase::Unstarted => SyncMessage::SlideAnnouncement {
                index,
                count,
                points_awarded: self.config.points_awarded,
                duration: self.config.introduce_slide,
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
                answers: self.get_answers_for_player(
                    watcher_id,
                    watcher_kind,
                    team_manager.map_or(1, |tm| tm.alive_team_size(watcher_id, &tunnel_finder)),
                    team_manager.map_or(0, |tm| tm.alive_team_index(watcher_id, &tunnel_finder)),
                    team_manager.is_some(),
                ),
                answered_count: get_answered_count(self),
                answer_mode: self.config.answer_mode,
            },
            Phase::AnswersResults => {
                let answer_count = self.per_index_answer_counts();

                SyncMessage::AnswersResults {
                    index,
                    count,
                    question: &self.config.title,
                    media: self.config.media.as_ref(),
                    answers: self.config.answers.iter().map(|a| a.content.as_ref()).collect_vec(),
                    results: self
                        .config
                        .answers
                        .iter()
                        .enumerate()
                        .map(|(i, a)| AnswerChoiceResult {
                            correct: a.correct,
                            count: *answer_count.get(&i).unwrap_or(&0),
                        })
                        .collect_vec(),
                }
            }
        }
    }

    /// Handles scheduled alarm messages for timed state transitions
    ///
    /// This method processes alarm messages that trigger automatic transitions
    /// between slide states at predetermined times, such as moving from question
    /// display to answer selection or from answers to results.
    ///
    /// # Arguments
    ///
    /// * `watchers` - Connection manager for all participants
    /// * `team_manager` - Optional team manager for team-based games
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `message` - The alarm message to process
    /// * `index` - Current slide index in the game
    ///
    /// # Returns
    ///
    /// A `SlideAction` indicating whether to stay on the current slide or advance
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    pub(crate) fn receive_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tunnel_finder: F,
        message: &crate::AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        if let crate::AlarmMessage::MultipleChoice(inner) = message {
            self.default_receive_alarm(
                inner.to,
                team_manager,
                watchers,
                schedule_message,
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
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        self.default_receive_host_next(
            leaderboard,
            watchers,
            team_manager,
            schedule_message,
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
        tunnel_finder: F,
    ) {
        let answer = match self.config.answer_mode {
            AnswerMode::SingleAnswer => {
                if let IncomingPlayerMessage::IndexAnswer(selected_answer_index) = message
                    && selected_answer_index < self.config.answers.len()
                {
                    Some(vec![selected_answer_index])
                } else {
                    None
                }
            }
            AnswerMode::MultipleAnswers => {
                if let IncomingPlayerMessage::IndexArrayAnswer(selected_answer_indices) = message
                    && !selected_answer_indices.is_empty()
                    && selected_answer_indices.iter().all(|&i| i < self.config.answers.len())
                    && selected_answer_indices
                        .iter()
                        .collect::<std::collections::HashSet<_>>()
                        .len()
                        == selected_answer_indices.len()
                {
                    Some(selected_answer_indices)
                } else {
                    None
                }
            }
        };

        if let Some(answer) = answer {
            self.record_answer(watcher_id, answer);
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}
