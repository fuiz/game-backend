//! Order/ranking question implementation
//!
//! This module implements the order question type for Fuiz games.
//! Order questions present a set of items that players must arrange
//! in a specific sequence. Players drag and drop or reorder items
//! to match the correct ordering, and scoring is based on how close
//! their arrangement is to the correct order.

use std::{collections::HashMap, time::Duration};

use garde::Validate;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_with::DurationMilliSeconds;
use web_time::SystemTime;

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
        AnswerHandler, QuestionReceiveMessage, SlideStateManager, SlideTimer, add_scores_to_leaderboard,
        all_players_answered, get_answered_count,
    },
    media::Media,
};

// Re-export SlideState publicly from slide_traits
pub use super::common::SlideState;

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
#[derive(Debug, Clone, Serialize, serde::Deserialize)]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Items in shuffled order as presented to players
    shuffled_answers: Vec<String>,
    /// Player arrangements with submission timestamps
    user_answers: HashMap<Id, (Vec<String>, SystemTime)>,
    /// Distinct live players who have answered. Maintained incrementally.
    #[serde(default)]
    live_answered_count: usize,
    /// Time when the ordering interface was first displayed
    answer_start: Option<SystemTime>,
    /// Current phase of the slide presentation
    state: SlideState,
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
            user_answers: HashMap::new(),
            live_answered_count: 0,
            answer_start: None,
            state: SlideState::Unstarted,
        }
    }
}

/// Messages sent to the listeners to update their pre-existing state with the slide state
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage {
    /// Announcement of the question without its answers
    QuestionAnnouncement {
        /// Index of the slide (0-indexing)
        index: usize,
        /// Total count of slides
        count: usize,
        /// Question text (i.e. what's being asked)
        question: String,
        /// Accompanying media
        media: Option<Media>,
        /// Time before answers will be released, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Announcement of the question with its answers
    AnswersAnnouncement {
        /// Labels for the axis
        axis_labels: AxisLabels,
        /// Answers in a shuffled order
        answers: Vec<String>,
        /// Time where players can answer the question, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// (HOST ONLY): Number of players who answered the question
    AnswersCount(usize),
    /// Results of the game including correct answers and statistics of how many they got chosen
    AnswersResults {
        /// Correct answers
        answers: Vec<String>,
        /// Statistics of how many players got it right and wrong
        results: (usize, usize),
    },
}

/// Alarm messages for timed events in order questions
///
/// These messages are used internally to trigger state transitions
/// at scheduled times during question presentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AlarmMessage {
    /// Triggers a transition from one slide state to another
    ProceedFromSlideIntoSlide {
        /// Index of the slide being transitioned
        index: usize,
        /// Target state to transition to
        to: SlideState,
    },
}

/// Messages sent to the listeners who lack preexisting state to synchronize their state.
///
/// See [`UpdateMessage`] for explaination of these fields.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage {
    /// Announcement of the question without its answers
    QuestionAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: String,
        /// Optional media content accompanying the question
        media: Option<Media>,
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
        question: String,
        /// Labels for the ordering axis
        axis_labels: AxisLabels,
        /// Optional media content accompanying the question
        media: Option<Media>,
        /// Items to be ordered in shuffled arrangement
        answers: Vec<String>,
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
        question: String,
        /// Labels for the ordering axis
        axis_labels: AxisLabels,
        /// Optional media content that accompanied the question
        media: Option<Media>,
        /// Items in the correct order
        answers: Vec<String>,
        /// Statistics: (correct_count, incorrect_count)
        results: (usize, usize),
    },
}

impl SlideStateManager for State {
    fn state(&self) -> SlideState {
        self.state
    }

    fn change_state(&mut self, before: SlideState, after: SlideState) -> bool {
        if self.state == before {
            self.state = after;
            true
        } else {
            false
        }
    }
}

impl SlideTimer for State {
    fn answer_start(&self) -> Option<SystemTime> {
        self.answer_start
    }

    fn set_answer_start(&mut self, time: Option<SystemTime>) {
        self.answer_start = time;
    }
}

impl AnswerHandler<Vec<String>> for State {
    fn user_answers(&self) -> &HashMap<Id, (Vec<String>, SystemTime)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut HashMap<Id, (Vec<String>, SystemTime)> {
        &mut self.user_answers
    }

    fn live_answered_count(&self) -> usize {
        self.live_answered_count
    }

    fn live_answered_count_mut(&mut self) -> &mut usize {
        &mut self.live_answered_count
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
        self.send_question_announcements(watchers, schedule_message, tunnel_finder, index, count);
    }

    /// Sends the initial question announcement to all participants
    ///
    /// This method handles the transition from Unstarted to Question state,
    /// announcing the question text and media without revealing the items to order.
    /// It schedules the transition to the ordering phase or immediately proceeds
    /// if no introduction time is configured.
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
    fn send_question_announcements<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        if self.change_state(SlideState::Unstarted, SlideState::Question) {
            watchers.announce(
                &UpdateMessage::QuestionAnnouncement {
                    index,
                    count,
                    question: self.config.title.clone(),
                    media: self.config.media.clone(),
                    duration: self.config.introduce_question,
                }
                .into(),
                &tunnel_finder,
            );

            if let Some(d) = self.config.introduce_question {
                if d.is_zero() {
                    self.send_answers_announcements(watchers, tunnel_finder, schedule_message, index, count);
                } else {
                    schedule_message(
                        AlarmMessage::ProceedFromSlideIntoSlide {
                            index,
                            to: SlideState::Answers,
                        }
                        .into(),
                        d,
                    );
                }
            }
        }
    }

    /// Transitions to the ordering phase and reveals shuffled items
    ///
    /// This method handles the transition from Question to Answers state,
    /// shuffling the items and revealing them to participants for ordering.
    /// It starts the ordering timer and schedules the transition to results.
    ///
    /// # Arguments
    ///
    /// * `watchers` - Connection manager for all participants
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `index` - Current slide index in the game
    /// * `_count` - Total number of slides in the game (unused)
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    fn send_answers_announcements<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        tunnel_finder: F,
        schedule_message: S,
        index: usize,
        _count: usize,
    ) {
        if self.change_state(SlideState::Question, SlideState::Answers) {
            self.shuffled_answers.clone_from(&self.config.answers);
            fastrand::shuffle(&mut self.shuffled_answers);

            self.start_timer();

            watchers.announce(
                &UpdateMessage::AnswersAnnouncement {
                    axis_labels: self.config.axis_labels.clone(),
                    answers: self.shuffled_answers.clone(),
                    duration: self.config.time_limit,
                }
                .into(),
                tunnel_finder,
            );

            if let Some(time_limit) = self.config.time_limit {
                schedule_message(
                    AlarmMessage::ProceedFromSlideIntoSlide {
                        index,
                        to: SlideState::AnswersResults,
                    }
                    .into(),
                    time_limit,
                );
            }
            // None = host-paced: no timer, host must press Next
        }
    }

    /// Sends the results showing correct order and player statistics
    ///
    /// This method handles the transition from Answers to `AnswersResults` state,
    /// revealing the correct order and showing statistics about player responses.
    ///
    /// # Arguments
    ///
    /// * `watchers` - Connection manager for all participants
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn send_answers_results<F: TunnelFinder>(&mut self, watchers: &Watchers, tunnel_finder: F) {
        if self.change_state(SlideState::Answers, SlideState::AnswersResults) {
            let correct_count = self.correct_count();

            watchers.announce(
                &UpdateMessage::AnswersResults {
                    answers: self.config.answers.iter().cloned().collect_vec(),
                    results: (correct_count, self.user_answers.len() - correct_count),
                }
                .into(),
                tunnel_finder,
            );
        }
    }

    /// Generates a synchronization message for a participant joining during the question
    ///
    /// This method creates the appropriate sync message based on the current slide state,
    /// allowing newly connected participants to see the current question state with
    /// correct timing and item arrangement.
    ///
    /// # Arguments
    ///
    /// * `_watcher_id` - ID of the participant to synchronize (unused)
    /// * `_watcher_kind` - Type of participant (unused)
    /// * `_team_manager` - Optional team manager for team-based games (unused)
    /// * `_watchers` - Connection manager for all participants (unused)
    /// * `_tunnel_finder` - Function to find communication tunnels (unused)
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
    ///
    /// # Returns
    ///
    /// A `SyncMessage` appropriate for the current state
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    pub fn state_message<F: TunnelFinder>(
        &self,
        _watcher_id: Id,
        _watcher_kind: ValueKind,
        _team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        _watchers: &Watchers,
        _tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SyncMessage {
        match self.state() {
            SlideState::Unstarted | SlideState::Question => SyncMessage::QuestionAnnouncement {
                index,
                count,
                question: self.config.title.clone(),
                media: self.config.media.clone(),
                duration: self.config.introduce_question.map(|d| d.saturating_sub(self.elapsed())),
            },
            SlideState::Answers => SyncMessage::AnswersAnnouncement {
                index,
                count,
                question: self.config.title.clone(),
                axis_labels: self.config.axis_labels.clone(),
                media: self.config.media.clone(),
                answers: self.shuffled_answers.clone(),
                duration: self.config.time_limit.map(|d| d.saturating_sub(self.elapsed())),
            },
            SlideState::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: self.config.title.clone(),
                axis_labels: self.config.axis_labels.clone(),
                media: self.config.media.clone(),
                answers: self.config.answers.clone(),
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
        _leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        _team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tunnel_finder: F,
        message: &crate::AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        if let crate::AlarmMessage::Order(AlarmMessage::ProceedFromSlideIntoSlide { index: _, to }) = message {
            match to {
                SlideState::Answers => {
                    self.send_answers_announcements(watchers, tunnel_finder, schedule_message, index, count);
                }
                SlideState::AnswersResults => {
                    self.send_answers_results(watchers, tunnel_finder);
                }
                _ => {}
            }
        }

        SlideAction::Stay
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
        match self.state() {
            SlideState::Unstarted => {
                self.send_question_announcements(watchers, schedule_message, tunnel_finder, index, count);
            }
            SlideState::Question => {
                self.send_answers_announcements(watchers, tunnel_finder, schedule_message, index, count);
            }
            SlideState::Answers => {
                self.send_answers_results(watchers, tunnel_finder);
            }
            SlideState::AnswersResults => {
                add_scores_to_leaderboard(self, self, leaderboard, watchers, team_manager, tunnel_finder);
                return SlideAction::Next { schedule_message };
            }
        }

        SlideAction::Stay
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
            if all_players_answered(self, watchers) {
                self.send_answers_results(watchers, &tunnel_finder);
            } else {
                watchers.announce_specific(
                    ValueKind::Host,
                    &UpdateMessage::AnswersCount(get_answered_count(self)).into(),
                    &tunnel_finder,
                );
            }
        }
    }
}
