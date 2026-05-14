//! Order/ranking question implementation
//!
//! This module implements the order question type for Fuiz games.
//! Order questions present a set of items that players must arrange
//! in a specific sequence. Players drag and drop or reorder items
//! to match the correct ordering, and scoring is based on how close
//! their arrangement is to the correct order.

use std::time::Duration;

use crate::time::Timestamp;
use garde::Validate;
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
        SlideStateManager, SlideTimer,
    },
    media::Media,
};

/// Lifecycle phases for an order slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without items to order.
    Question,
    /// Showing shuffled items and accepting player arrangements.
    Answers,
    /// Displaying results with the correct order and statistics.
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

/// Labels for the ordering axis in an order question
///
/// These labels help players understand what the ordering represents,
/// such as "Earliest" to "Latest" or "Smallest" to "Largest".
#[derive(Debug, Clone, Default, Serialize, serde::Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct AxisLabels {
    /// Label for the start/left end of the ordering axis
    #[garde(length(chars, max = ctx.order.max_label_length))]
    from: Option<String>,
    /// Label for the end/right end of the ordering axis
    #[garde(length(chars, max = ctx.order.max_label_length))]
    to: Option<String>,
}

/// Configuration for an order question slide
///
/// Contains all the settings and content for a single order question,
/// including the question text, media, timing, items to be ordered,
/// and axis labels for the ordering interface.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The question title, represents what's being asked
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Accompanying media
    #[garde(dive)]
    media: Option<Media>,
    /// Time before the question is displayed.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can answer the question.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Maximum number of points awarded the question, decreases linearly to half the amount by the end of the slide
    #[garde(skip)]
    points_awarded: u64,
    /// Accompanying answers in the correct order
    #[garde(length(max = ctx.order.max_answer_count),
        inner(length(chars, max = ctx.answer_text.max_length))
    )]
    answers: Vec<String>,
    /// From and to labels for the order
    #[garde(dive)]
    axis_labels: AxisLabels,
}

/// Runtime state for an order question during gameplay
///
/// This struct maintains the dynamic state of an order question as it
/// progresses through its phases, tracking player arrangements, timing
/// information, shuffled item order, and current presentation state.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, serde::Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Items in shuffled order as presented to players
    shuffled_answers: Vec<String>,
    /// Player arrangements with submission timestamps
    user_answers: FxHashMap<Id, (Vec<String>, Timestamp)>,
    /// Shared runtime core: slide phase, answer-start timestamp, live-answered tally.
    #[cfg_attr(feature = "serializable", serde(flatten))]
    core: SlideCore<Phase>,
}

impl SlideConfig {
    /// Creates a new runtime state from this configuration
    ///
    /// This method initializes a fresh state for gameplay, setting up
    /// empty answer tracking, unshuffled items, and the initial unstarted phase.
    ///
    /// # Returns
    ///
    /// A new `State` ready for gameplay
    pub fn to_state(&self) -> State {
        State {
            config: self.clone(),
            shuffled_answers: Vec::new(),
            user_answers: FxHashMap::default(),
            core: SlideCore::default(),
        }
    }
}

/// Messages sent to the listeners to update their pre-existing state with the slide state
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Announcement of the question without its answers
    QuestionAnnouncement {
        /// Index of the slide (0-indexing)
        index: usize,
        /// Total count of slides
        count: usize,
        /// Question text (i.e. what's being asked)
        question: &'a str,
        /// Accompanying media
        media: Option<&'a Media>,
        /// Time before answers will be released, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Announcement of the question with its answers
    AnswersAnnouncement {
        /// Labels for the axis
        axis_labels: &'a AxisLabels,
        /// Answers in a shuffled order
        answers: &'a [String],
        /// Time where players can answer the question, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// (HOST ONLY): Number of players who answered the question
    AnswersCount(usize),
    /// Results of the game including correct answers and statistics of how many they got chosen
    AnswersResults {
        /// Correct answers
        answers: &'a [String],
        /// Statistics of how many players got it right and wrong
        results: (usize, usize),
    },
}

/// Scheduled phase-transition alarm for order slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to the listeners who lack preexisting state to synchronize their state.
///
/// See [`UpdateMessage`] for explaination of these fields.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage<'a> {
    /// Announcement of the question without its answers
    QuestionAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Remaining time for the question to be displayed without its answers, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Announcement of the question with its answers
    AnswersAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Labels for the ordering axis
        axis_labels: &'a AxisLabels,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Items to be ordered in shuffled arrangement
        answers: &'a [String],
        /// Time where players can answer the question, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Results of the game including correct answers and statistics of how many they got chosen
    AnswersResults {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text that was asked
        question: &'a str,
        /// Labels for the ordering axis
        axis_labels: &'a AxisLabels,
        /// Optional media content that accompanied the question
        media: Option<&'a Media>,
        /// Items in the correct order
        answers: &'a [String],
        /// Statistics: (correct_count, incorrect_count)
        results: (usize, usize),
    },
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

impl AnswerHandler<Vec<String>> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (Vec<String>, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (Vec<String>, Timestamp)> {
        &mut self.user_answers
    }

    fn is_correct_answer(&self, answer: &Vec<String>) -> bool {
        answer == &self.config.answers
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
            let correct_count = self.correct_count();
            watchers.announce(
                &UpdateMessage::AnswersResults {
                    answers: &self.config.answers,
                    results: (correct_count, self.user_answers.len() - correct_count),
                }
                .into(),
                tunnel_finder,
            );
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
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        match phase {
            Phase::Unstarted => {}
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
                if let Some(d) = self.config.introduce_question {
                    if d.is_zero() {
                        self.enter_phase(
                            Phase::Answers,
                            None,
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
                            d,
                        );
                    }
                }
            }
            Phase::Answers => {
                if !self.change_state(Phase::Question, Phase::Answers) {
                    return;
                }
                self.shuffled_answers.clone_from(&self.config.answers);
                fastrand::shuffle(&mut self.shuffled_answers);

                self.start_timer();
                self.reserve_for_players(watchers.specific_count(ValueKind::Player));

                watchers.announce(
                    &UpdateMessage::AnswersAnnouncement {
                        axis_labels: &self.config.axis_labels,
                        answers: &self.shuffled_answers,
                        duration: self.config.time_limit,
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
    /// Starts the order slide by sending initial question announcements
    ///
    /// This method initiates the question flow by transitioning to the question phase
    /// and announcing the question to all participants. It schedules the transition
    /// to the ordering phase based on the configured introduction duration.
    ///
    /// # Arguments
    ///
    /// * `watchers` - Connection manager for all participants
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        self.enter_phase(
            Phase::Question,
            None,
            watchers,
            schedule_message,
            tunnel_finder,
            index,
            count,
        );
    }

    /// Generates a synchronization message for a participant joining during the question
    ///
    /// This method creates the appropriate sync message based on the current slide state,
    /// allowing newly connected participants to see the current question state with
    /// correct timing and item arrangement.
    ///
    /// # Arguments
    ///
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
    ///
    /// # Returns
    ///
    /// A `SyncMessage` appropriate for the current state
    pub fn state_message(&self, index: usize, count: usize) -> SyncMessage<'_> {
        match self.state() {
            Phase::Unstarted | Phase::Question => SyncMessage::QuestionAnnouncement {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                duration: self.config.introduce_question.map(|d| d.saturating_sub(self.elapsed())),
            },
            Phase::Answers => SyncMessage::AnswersAnnouncement {
                index,
                count,
                question: &self.config.title,
                axis_labels: &self.config.axis_labels,
                media: self.config.media.as_ref(),
                answers: &self.shuffled_answers,
                duration: self.config.time_limit.map(|d| d.saturating_sub(self.elapsed())),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                axis_labels: &self.config.axis_labels,
                media: self.config.media.as_ref(),
                answers: &self.config.answers,
                results: {
                    let correct_count = self.correct_count();
                    (correct_count, self.user_answers.len() - correct_count)
                },
            },
        }
    }

    /// Handles scheduled alarm messages for timed state transitions
    ///
    /// This method processes alarm messages that trigger automatic transitions
    /// between slide states at predetermined times, such as moving from question
    /// display to item ordering or from ordering to results.
    ///
    /// # Arguments
    ///
    /// * `_leaderboard` - Mutable reference to the game leaderboard (unused)
    /// * `watchers` - Connection manager for all participants
    /// * `_team_manager` - Optional team manager for team-based games (unused)
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `message` - The alarm message to process
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
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
        schedule_message: S,
        tunnel_finder: F,
        message: &crate::AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        if let crate::AlarmMessage::Order(inner) = message {
            self.default_receive_alarm(inner.to, None, watchers, schedule_message, tunnel_finder, index, count)
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
        if let IncomingPlayerMessage::StringArrayAnswer(v) = message {
            self.record_answer(watcher_id, v);
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}
