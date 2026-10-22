//! Slider (numeric estimate) question implementation
//!
//! Players drag a slider along a numeric range and submit a value. The answer
//! is correct when it lands within `tolerance` of the configured value, so the
//! type works both for exact-value questions (`tolerance: 0`) and for
//! "how close can you get" estimates.
//!
//! Submitted values are snapped to the configured `step` grid and clamped to
//! `[min, max]` server-side, so the results histogram always has one bucket per
//! reachable value regardless of what a client sends.

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

/// Lifecycle phases for a slider slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without the slider.
    Question,
    /// Showing the slider and accepting player values.
    Answers,
    /// Displaying the correct value and the distribution of guesses.
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

/// The numeric range a slider spans, plus the granularity players move in.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Range {
    /// Lowest selectable value.
    pub min: f64,
    /// Highest selectable value.
    pub max: f64,
    /// Distance between two adjacent selectable values.
    pub step: f64,
}

impl Default for Range {
    fn default() -> Self {
        Self {
            min: 0.0,
            max: 100.0,
            step: 1.0,
        }
    }
}

impl Range {
    /// Number of selectable values, or `None` when the range is degenerate.
    fn steps(&self) -> Option<usize> {
        if !self.min.is_finite() || !self.max.is_finite() || !self.step.is_finite() {
            return None;
        }
        if self.step <= 0.0 || self.max < self.min {
            return None;
        }
        let count = ((self.max - self.min) / self.step).floor();
        if count.is_finite() && count >= 0.0 {
            Some(count as usize + 1)
        } else {
            None
        }
    }

    /// How many decimals a value on this range needs, taken from the step: a
    /// slider of tenths reads "3.4", not "3.4000000000000004". Matches the
    /// website's `formatSliderValue`.
    fn decimals(&self) -> usize {
        let step = self.step.to_string();
        step.split_once('.').map_or(0, |(_, fraction)| fraction.len().min(6))
    }

    /// Clamps `value` into the range and snaps it to the nearest grid point.
    ///
    /// Snapping means every recorded answer is one of the values the slider can
    /// actually stop on, so the results histogram never grows past `steps()`
    /// buckets even if a client posts arbitrary floats.
    fn snap(&self, value: f64) -> f64 {
        if !value.is_finite() {
            return self.min;
        }
        let clamped = value.clamp(self.min, self.max);
        let index = ((clamped - self.min) / self.step).round();
        let snapped = self.min + index * self.step;
        // A range whose span isn't a whole number of steps (0..7 by 2) tops out
        // at 6; rounding 7 up to 8 would invent a stop the slider can't reach.
        if snapped > self.max {
            self.min + (index - 1.0).max(0.0) * self.step
        } else {
            snapped
        }
    }
}

/// Configuration for a slider slide
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The question title, represents what's being asked
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Accompanying media
    #[garde(dive)]
    media: Option<Media>,
    /// Duration of the slide-announcement intro shown before the question: an
    /// animation naming the question type and its scoring. Absent means a
    /// default duration; `null` is host-paced (skip manually); a value auto-advances
    /// after it. The host can always skip early.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_slide(val)))]
    #[serde(
        default = "crate::fuiz::common::default_introduce_slide",
        with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>"
    )]
    introduce_slide: Option<Duration>,
    /// Time before the slider is displayed.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can move the slider.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Maximum number of points awarded, decreasing linearly to half by the end of the slide
    #[garde(skip)]
    points_awarded: u64,
    /// The span the slider covers and the granularity it moves in
    #[garde(custom(|val, ctx: &crate::settings::Settings| validate_range(val, ctx)))]
    #[serde(default)]
    range: Range,
    /// The value that earns points
    #[garde(skip)]
    correct: f64,
    /// How far from `correct` still counts as correct. `0` demands an exact hit.
    #[garde(skip)]
    #[serde(default)]
    tolerance: f64,
    /// Unit displayed next to the value, e.g. `%`, `kg`, `deg C`
    #[garde(length(chars, max = ctx.slider.max_unit_length))]
    #[serde(default)]
    unit: Option<String>,
}

fn validate_range(range: &Range, ctx: &crate::settings::Settings) -> garde::Result {
    match range.steps() {
        None => Err(garde::Error::new("slider range must have max >= min and step > 0")),
        Some(steps) if steps > ctx.slider.max_steps => Err(garde::Error::new(format!(
            "slider has {steps} stops, more than the maximum of {}",
            ctx.slider.max_steps
        ))),
        Some(_) => Ok(()),
    }
}

/// Runtime state for a slider slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Player values (already snapped to the step grid) with submission timestamps
    user_answers: FxHashMap<Id, (f64, Timestamp)>,
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

/// How many players landed on a given slider value.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ValueCount {
    /// The value players landed on.
    pub value: f64,
    /// How many players chose it.
    pub count: usize,
}

/// Aggregated results for a slider slide.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    /// One entry per distinct submitted value, ascending.
    pub distribution: Vec<ValueCount>,
    /// Mean of all submitted values, or `None` when nobody answered.
    pub average: Option<f64>,
    /// Number of players whose value fell within `tolerance`.
    pub correct_count: usize,
    /// Number of players who answered at all.
    pub total_count: usize,
}

/// Messages sent to listeners to update their pre-existing slider state.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Announces the upcoming question's type and scoring (the `Unstarted` phase).
    SlideAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// Maximum points awarded for a correct answer
        points_awarded: u64,
        /// Duration of the intro, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Announces the question without the slider
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Time before the slider appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Reveals the slider and opens the answering window
    AnswersAnnouncement {
        /// Time before the answering window closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The span the slider covers
        range: Range,
        /// Unit displayed next to the value
        unit: Option<&'a str>,
    },
    /// (HOST ONLY) Reports the number of players who have submitted values
    AnswersCount(usize),
    /// Shows the correct value alongside the distribution of guesses
    AnswersResults {
        /// The span the slider covered
        range: Range,
        /// Unit displayed next to the value
        unit: Option<&'a str>,
        /// The value that earned points
        correct: f64,
        /// How far from `correct` still counted as correct
        tolerance: f64,
        /// Aggregated player values
        results: Results,
    },
}

/// Scheduled phase-transition alarm for slider slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing slider state.
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
        /// Maximum points awarded for a correct answer
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
        /// Remaining time before the slider appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Synchronizes the answering phase
    AnswersAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Remaining answering time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The span the slider covers
        range: Range,
        /// Unit displayed next to the value
        unit: Option<&'a str>,
        /// Number of players who have already answered
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
        /// The span the slider covered
        range: Range,
        /// Unit displayed next to the value
        unit: Option<&'a str>,
        /// The value that earned points
        correct: f64,
        /// How far from `correct` still counted as correct
        tolerance: f64,
        /// Aggregated player values
        results: Results,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<f64> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (f64, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (f64, Timestamp)> {
        &mut self.user_answers
    }

    fn transform_answer(&self, answer: f64) -> f64 {
        self.config.range.snap(answer)
    }

    fn is_correct_answer(&self, answer: &f64) -> bool {
        (answer - self.config.correct).abs() <= self.config.tolerance + f64::EPSILON
    }

    fn describe_answer(&self, answer: &f64) -> String {
        let value = format!("{answer:.*}", self.config.range.decimals());
        match self.config.unit.as_deref() {
            Some(unit) => format!("{value}{unit}"),
            None => value,
        }
    }

    fn question_text(&self) -> &str {
        &self.config.title
    }

    fn is_graded(&self) -> bool {
        true
    }

    fn describe_correct_answer(&self) -> Option<String> {
        let correct = self.describe_answer(&self.config.correct);
        Some(if self.config.tolerance > 0.0 {
            format!("{correct} ± {}", self.describe_answer(&self.config.tolerance))
        } else {
            correct
        })
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
                    range: self.config.range,
                    unit: self.config.unit.as_deref(),
                    correct: self.config.correct,
                    tolerance: self.config.tolerance,
                    results: self.results(),
                }
                .into(),
                tunnel_finder,
            );
        }
    }
}

impl PhasedSlide<f64> for State {
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
                        range: self.config.range,
                        unit: self.config.unit.as_deref(),
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
    /// phase), then auto-advances after `introduce_slide`: immediately if
    /// zero, never if `None` (host-paced).
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

    /// Groups the submitted values into an ascending histogram.
    ///
    /// Answers are snapped on the way in, so grouping by step index is exact
    /// rather than a float-equality gamble.
    fn results(&self) -> Results {
        let step = self.config.range.step;
        let min = self.config.range.min;

        let mut per_step: FxHashMap<i64, usize> = FxHashMap::default();
        let mut sum = 0.0;
        let mut correct_count = 0;
        for (value, _) in self.user_answers.values() {
            let bucket = ((value - min) / step).round() as i64;
            *per_step.entry(bucket).or_default() += 1;
            sum += value;
            if self.is_correct_answer(value) {
                correct_count += 1;
            }
        }

        let total_count = self.user_answers.len();

        Results {
            distribution: per_step
                .into_iter()
                .sorted_by_key(|(bucket, _)| *bucket)
                .map(|(bucket, count)| ValueCount {
                    value: self.config.range.snap(min + bucket as f64 * step),
                    count,
                })
                .collect_vec(),
            average: (total_count > 0).then(|| sum / total_count as f64),
            correct_count,
            total_count,
        }
    }

    /// Starts the slider slide by entering the [`Phase::Unstarted`] phase.
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
                range: self.config.range,
                unit: self.config.unit.as_deref(),
                answered_count: get_answered_count(self),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                range: self.config.range,
                unit: self.config.unit.as_deref(),
                correct: self.config.correct,
                tolerance: self.config.tolerance,
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
        if let crate::AlarmMessage::Slider(inner) = message {
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
        if let IncomingPlayerMessage::NumberAnswer(value) = message
            && value.is_finite()
        {
            self.record_answer_at(watcher_id, value, tick.now());
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Slider values ride through float arithmetic, so compare them by
    /// closeness rather than by bit pattern.
    #[track_caller]
    fn assert_value(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "expected {expected}, got {actual}");
    }

    fn config(range: Range, correct: f64, tolerance: f64) -> SlideConfig {
        SlideConfig {
            title: "How tall?".to_string(),
            media: None,
            introduce_slide: None,
            introduce_question: None,
            time_limit: None,
            points_awarded: 1000,
            range,
            correct,
            tolerance,
            unit: None,
        }
    }

    #[test]
    fn answers_are_described_to_the_step_not_the_float() {
        let range = Range {
            min: 0.0,
            max: 1.0,
            step: 0.1,
        };
        let state = config(range, 0.5, 0.1).to_state();
        // 0.1 * 3 is 0.30000000000000004 in floating point.
        assert_eq!(state.describe_answer(&range.snap(0.3)), "0.3");
        assert_eq!(state.describe_correct_answer().as_deref(), Some("0.5 ± 0.1"));
    }

    #[test]
    fn snap_clamps_and_rounds_to_the_step_grid() {
        let range = Range {
            min: 0.0,
            max: 10.0,
            step: 2.0,
        };
        assert_value(range.snap(-5.0), 0.0);
        assert_value(range.snap(100.0), 10.0);
        assert_value(range.snap(3.4), 4.0);
        assert_value(range.snap(2.9), 2.0);
    }

    #[test]
    fn snap_never_exceeds_max_on_a_ragged_grid() {
        // 0..7 in steps of 2 stops at 6; a value of 7 must not snap to 8.
        let range = Range {
            min: 0.0,
            max: 7.0,
            step: 2.0,
        };
        assert_value(range.snap(7.0), 6.0);
        assert_value(range.snap(100.0), 6.0);
    }

    #[test]
    fn steps_rejects_degenerate_ranges() {
        assert!(
            Range {
                min: 0.0,
                max: 1.0,
                step: 0.0
            }
            .steps()
            .is_none()
        );
        assert!(
            Range {
                min: 5.0,
                max: 1.0,
                step: 1.0
            }
            .steps()
            .is_none()
        );
        assert_eq!(
            Range {
                min: 0.0,
                max: 10.0,
                step: 1.0
            }
            .steps(),
            Some(11)
        );
    }

    #[test]
    fn tolerance_widens_the_correct_band() {
        let exact = config(Range::default(), 50.0, 0.0).to_state();
        assert!(exact.is_correct_answer(&50.0));
        assert!(!exact.is_correct_answer(&51.0));

        let loose = config(Range::default(), 50.0, 5.0).to_state();
        assert!(loose.is_correct_answer(&45.0));
        assert!(loose.is_correct_answer(&55.0));
        assert!(!loose.is_correct_answer(&56.0));
    }

    #[test]
    fn results_bucket_by_value_and_average() {
        let mut state = config(Range::default(), 50.0, 0.0).to_state();
        state.record_answer(Id::new(), 10.0);
        state.record_answer(Id::new(), 10.0);
        state.record_answer(Id::new(), 50.0);

        let results = state.results();
        assert_eq!(results.total_count, 3);
        assert_eq!(results.correct_count, 1);
        assert_eq!(
            results.distribution.iter().map(|v| v.count).collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_value(results.distribution[0].value, 10.0);
        assert_value(results.distribution[1].value, 50.0);
        assert!((results.average.expect("answers exist") - 70.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn results_are_empty_without_answers() {
        let state = config(Range::default(), 50.0, 0.0).to_state();
        let results = state.results();
        assert_eq!(results.total_count, 0);
        assert!(results.distribution.is_empty());
        assert!(results.average.is_none());
    }

    #[test]
    fn recorded_answers_are_snapped() {
        let mut state = config(
            Range {
                min: 0.0,
                max: 10.0,
                step: 5.0,
            },
            5.0,
            0.0,
        )
        .to_state();
        let id = Id::new();
        state.record_answer(id, 7.4);
        assert_value(state.user_answers().get(&id).map(|(v, _)| *v).expect("recorded"), 5.0);
    }
}
