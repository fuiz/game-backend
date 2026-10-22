//! Poll (opinion choice) question implementation
//!
//! A poll looks like a multiple choice question but has no right answer: every
//! player picks one option and the room sees how the votes fell. Because
//! nothing is correct, no points are awarded, but the slide still records a zero
//! for every player so per-slide point arrays stay aligned with slide indices.

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
    config::{TextOrMedia, TextOrMediaRef},
    media::Media,
};

/// Lifecycle phases for a poll slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without the options.
    Question,
    /// Showing the options and accepting votes.
    Answers,
    /// Displaying how the votes fell.
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

/// Configuration for a poll slide
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
    /// Time before the options are displayed.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can vote.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Points awarded. Polls collect opinions, so this is `0` in practice; the
    /// field exists so every slide type announces its scoring the same way.
    #[garde(skip)]
    #[serde(default)]
    points_awarded: u64,
    /// The options players vote between
    #[garde(length(max = ctx.poll.max_answer_count), dive)]
    answers: Vec<TextOrMedia>,
}

/// Runtime state for a poll slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// The option each player voted for, with submission timestamps
    user_answers: FxHashMap<Id, (usize, Timestamp)>,
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

/// Aggregated results for a poll slide.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    /// One count per option, aligned with the configured order.
    pub counts: Vec<usize>,
    /// Number of players who voted.
    pub total_count: usize,
}

/// Messages sent to listeners to update their pre-existing poll state.
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
    },
    /// Announces the question without the options
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Time before the options appear, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Reveals the options and opens voting
    AnswersAnnouncement {
        /// Time before voting closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The options players vote between
        answers: Vec<TextOrMediaRef<'a>>,
    },
    /// (HOST ONLY) Reports the number of players who have voted
    AnswersCount(usize),
    /// Shows how the votes fell
    AnswersResults {
        /// The options players voted between
        answers: Vec<TextOrMediaRef<'a>>,
        /// Aggregated votes
        results: Results,
    },
}

/// Scheduled phase-transition alarm for poll slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing poll state.
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
        /// Remaining time before the options appear, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Synchronizes the voting phase
    AnswersAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Remaining voting time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The options players vote between
        answers: Vec<TextOrMediaRef<'a>>,
        /// Number of players who have already voted
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
        /// The options players voted between
        answers: Vec<TextOrMediaRef<'a>>,
        /// Aggregated votes
        results: Results,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<usize> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (usize, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (usize, Timestamp)> {
        &mut self.user_answers
    }

    /// Opinions have no right answer, so nothing scores.
    fn is_correct_answer(&self, _answer: &usize) -> bool {
        false
    }

    fn describe_answer(&self, answer: &usize) -> String {
        option_text(&self.config.answers, *answer)
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
            watchers.announce(
                &UpdateMessage::AnswersResults {
                    answers: self.answers(),
                    results: self.results(),
                }
                .into(),
                tunnel_finder,
            );
        }
    }
}

impl PhasedSlide<usize> for State {
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
                        answers: self.answers(),
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

    /// Borrowed view of the configured options for outgoing messages.
    fn answers(&self) -> Vec<TextOrMediaRef<'_>> {
        self.config.answers.iter().map(TextOrMedia::as_ref).collect_vec()
    }

    /// Tallies one bucket per configured option.
    fn results(&self) -> Results {
        let mut counts = vec![0_usize; self.config.answers.len()];
        for (choice, _) in self.user_answers.values() {
            if let Some(slot) = counts.get_mut(*choice) {
                *slot += 1;
            }
        }
        Results {
            counts,
            total_count: self.user_answers.len(),
        }
    }

    /// Starts the poll slide by entering the [`Phase::Unstarted`] phase.
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
                answers: self.answers(),
                answered_count: get_answered_count(self),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                answers: self.answers(),
                results: self.results(),
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
        if let crate::AlarmMessage::Poll(inner) = message {
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
        if let IncomingPlayerMessage::IndexAnswer(choice) = message
            && choice < self.config.answers.len()
        {
            self.record_answer_at(watcher_id, choice, tick.now());
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}

/// The wording of an option, so a host reads a choice rather than an index.
fn option_text(answers: &[TextOrMedia], index: usize) -> String {
    match answers.get(index) {
        Some(TextOrMedia::Text(text)) => text.clone(),
        // An image option speaks through its alt text when the host gave it any.
        Some(TextOrMedia::Media(media)) if !media.alt().trim().is_empty() => media.alt().to_owned(),
        Some(TextOrMedia::Media(_)) | None => format!("#{}", index + 1),
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn config(options: usize) -> SlideConfig {
        SlideConfig {
            title: "Favourite season?".to_string(),
            media: None,
            introduce_slide: None,
            introduce_question: None,
            time_limit: None,
            points_awarded: 0,
            answers: (0..options).map(|i| TextOrMedia::Text(format!("Option {i}"))).collect(),
        }
    }

    #[test]
    fn counts_align_with_the_configured_options() {
        let mut state = config(3).to_state();
        state.record_answer(Id::new(), 0);
        state.record_answer(Id::new(), 2);
        state.record_answer(Id::new(), 2);

        let results = state.results();
        assert_eq!(results.counts, vec![1, 0, 2]);
        assert_eq!(results.total_count, 3);
    }

    #[test]
    fn out_of_range_votes_do_not_land_in_a_bucket() {
        // `receive_player_message` rejects these, but `results` must stay total
        // even if a stale answer survives a config change on reload.
        let mut state = config(2).to_state();
        state.record_answer(Id::new(), 7);
        assert_eq!(state.results().counts, vec![0, 0]);
    }

    #[test]
    fn a_second_vote_replaces_the_first() {
        let mut state = config(3).to_state();
        let id = Id::new();
        state.record_answer(id, 0);
        state.record_answer(id, 1);

        assert_eq!(state.results().counts, vec![0, 1, 0]);
        assert_eq!(state.live_answered_count(), 1);
    }

    #[test]
    fn opinions_never_score() {
        let state = config(3).to_state();
        assert!(!state.is_correct_answer(&0));
        assert_eq!(state.max_points(), 0);
    }
}
