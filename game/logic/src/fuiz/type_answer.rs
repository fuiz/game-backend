//! Type answer (free text) question implementation
//!
//! This module implements the type answer question type for Fuiz games.
//! Type answer questions present a question and allow players to submit
//! free text responses. The system supports multiple acceptable answers
//! and uses fuzzy matching to determine correctness.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

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
        SlideStateManager, SlideTimer,
    },
    media::Media,
};

/// Lifecycle phases for a type-answer slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without accepting answers.
    Question,
    /// Accepting answers from players.
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

/// Configuration for a type answer slide
///
/// Contains all the settings and content for a single type answer question,
/// including the question text, media, timing, and acceptable answers.
#[derive(Debug, Clone, Serialize, serde::Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The question title, represents what's being asked
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Accompanying media
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
    /// Time before the answers are displayed.
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
    /// List of acceptable text answers for this question
    #[garde(length(max = ctx.type_answer.max_answer_count),
        inner(length(chars, max = ctx.answer_text.max_length))
    )]
    answers: Vec<String>,
    /// Whether answer matching should be case-sensitive
    #[garde(skip)]
    #[serde(default)]
    case_sensitive: bool,
}

/// Runtime state for a type answer slide
///
/// Tracks the current state of the slide including player answers,
/// timing information, and the current phase of the question.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, serde::Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Player text answers with submission timestamps
    user_answers: FxHashMap<Id, (String, Timestamp)>,
    /// Shared runtime core: slide phase, answer-start timestamp, live-answered tally.
    #[cfg_attr(feature = "serializable", serde(flatten))]
    core: SlideCore<Phase>,
    /// The set of cleaned player answers
    cleaned_answers: HashSet<String>,
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
            user_answers: HashMap::default(),
            core: SlideCore::default(),
            cleaned_answers: self
                .answers
                .iter()
                .map(|a| clean_answer(a, self.case_sensitive))
                .collect(),
        }
    }
}

/// Messages sent to the listeners to update their pre-existing state with the slide state
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
        /// Accept answers from players
        accept_answers: bool,
    },
    /// (HOST ONLY): Number of players who answered the question
    AnswersCount(usize),
    /// Results of the game including correct answers and statistics of how many they got chosen
    AnswersResults {
        /// Correct answers
        answers: Vec<&'a str>,
        /// Statistics of how many times each answer was chosen
        results: Vec<(&'a str, usize)>,
        /// Case-sensitive check for answers
        case_sensitive: bool,
    },
}

/// Scheduled phase-transition alarm for type-answer slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to the listeners who lack preexisting state to synchronize their state.
///
/// See [`UpdateMessage`] for explaination of these fields.
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
        /// Whether to accept text answers from players
        accept_answers: bool,
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
        /// Correct answers for this question
        answers: Vec<&'a str>,
        /// Statistics of player submissions: (answer_text, count)
        results: Vec<(&'a str, usize)>,
        /// Whether the answer matching was case-sensitive
        case_sensitive: bool,
    },
}

/// Normalizes an answer string for comparison
///
/// # Arguments
/// * `answer` - The answer string to clean
/// * `case_sensitive` - Whether to preserve case sensitivity
///
/// # Returns
/// * Cleaned answer string (trimmed and optionally lowercased)
fn clean_answer(answer: &str, case_sensitive: bool) -> String {
    if case_sensitive {
        answer.trim().to_string()
    } else {
        answer.trim().to_lowercase()
    }
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

impl AnswerHandler<String> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (String, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (String, Timestamp)> {
        &mut self.user_answers
    }

    fn transform_answer(&self, answer: String) -> String {
        clean_answer(&answer, self.config.case_sensitive)
    }

    fn is_correct_answer(&self, answer: &String) -> bool {
        self.cleaned_answers.contains(answer)
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
            watchers.announce(
                &UpdateMessage::AnswersResults {
                    answers: self.cleaned_answers.iter().map(String::as_str).collect_vec(),
                    results: self
                        .user_answers
                        .values()
                        .map(|(a, _)| a.as_str())
                        .counts()
                        .into_iter()
                        .collect_vec(),
                    case_sensitive: self.config.case_sensitive,
                }
                .into(),
                tunnel_finder,
            );
        }
    }
}

impl PhasedSlide<String> for State {
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
            Phase::Unstarted => {
                self.announce_slide(watchers, schedule_message, tunnel_finder, index, count);
            }
            Phase::Question => {
                if !self.change_state(Phase::Unstarted, Phase::Question) {
                    return;
                }
                if let Some(duration) = self.config.introduce_question
                    && duration.is_zero()
                {
                    self.enter_phase(
                        Phase::Answers,
                        None,
                        watchers,
                        schedule_message,
                        tunnel_finder,
                        index,
                        count,
                    );
                    return;
                }

                self.start_timer();

                watchers.announce(
                    &UpdateMessage::QuestionAnnouncement {
                        index,
                        count,
                        question: &self.config.title,
                        media: self.config.media.as_ref(),
                        duration: self.config.introduce_question,
                        accept_answers: false,
                    }
                    .into(),
                    tunnel_finder,
                );

                if let Some(duration) = self.config.introduce_question {
                    schedule_message(
                        AlarmMessage {
                            index,
                            to: Phase::Answers,
                        }
                        .into(),
                        duration,
                    );
                }
                // None = host-paced: no timer, host must press Next.
            }
            Phase::Answers => {
                if !self.change_state(Phase::Question, Phase::Answers) {
                    return;
                }
                self.start_timer();
                self.reserve_for_players(watchers.specific_count(ValueKind::Player));

                watchers.announce(
                    &UpdateMessage::QuestionAnnouncement {
                        index,
                        count,
                        question: &self.config.title,
                        media: self.config.media.as_ref(),
                        duration: self.config.time_limit,
                        accept_answers: true,
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
    /// Announces the upcoming question's type and scoring (the `Unstarted`
    /// phase), then auto-advances to the question after `introduce_slide` —
    /// immediately if zero, never if `None` (host-paced).
    fn announce_slide<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
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
                None,
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

    /// Starts the type answer slide by entering the [`Phase::Question`] phase.
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        self.enter_phase(
            Phase::Unstarted,
            None,
            watchers,
            schedule_message,
            tunnel_finder,
            index,
            count,
        );
    }

    /// Synchronization message for a newly connected watcher, derived from the
    /// current phase.
    pub fn state_message(&self, index: usize, count: usize) -> SyncMessage<'_> {
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
                accept_answers: false,
            },
            Phase::Answers => SyncMessage::QuestionAnnouncement {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                duration: self
                    .config
                    .time_limit
                    .map(|duration| duration.saturating_sub(self.elapsed())),
                accept_answers: true,
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                answers: self.cleaned_answers.iter().map(String::as_str).collect_vec(),
                results: self
                    .user_answers
                    .values()
                    .map(|(a, _)| a.as_str())
                    .counts()
                    .into_iter()
                    .collect_vec(),
                case_sensitive: self.config.case_sensitive,
            },
        }
    }

    /// Forwards a phase-transition alarm to [`PhasedSlide::default_receive_alarm`].
    pub(crate) fn receive_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tunnel_finder: F,
        message: &crate::AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        if let crate::AlarmMessage::TypeAnswer(inner) = message {
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
        if let IncomingPlayerMessage::StringAnswer(v) = message {
            self.record_answer(watcher_id, v);
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}
