//! Scale (opinion rating) question implementation
//!
//! Players pick a whole number on a labelled scale. Two flavours share this
//! module because they differ only in presentation and in the summary statistic
//! the host is shown:
//!
//! - [`Style::Agreement`]: a short scale (typically 1-5) between two opposing
//!   labels, reported as an average.
//! - [`Style::Nps`]: the 0-10 Net Promoter Score scale, additionally reported
//!   as promoters / passives / detractors and the resulting NPS.
//!
//! Scales collect opinions, so no answer is "correct" and no points are
//! awarded; the slide still records a zero for every player so per-slide point
//! arrays stay aligned with the slide indices.

use std::time::Duration;

use garde::Validate;
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

/// Lifecycle phases for a scale slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without the scale.
    Question,
    /// Showing the scale and accepting player ratings.
    Answers,
    /// Displaying the distribution of ratings.
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

/// Which flavour of scale this slide presents.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Style {
    /// A short opinion scale between two opposing labels.
    #[default]
    Agreement,
    /// The 0-10 Net Promoter Score scale, reported with NPS statistics.
    Nps,
}

/// Labels shown beneath the ends (and optionally the middle) of the scale.
#[derive(Debug, Clone, Default, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct Labels {
    /// Label under the lowest point, e.g. "Strongly disagree".
    #[garde(length(chars, max = ctx.scale.max_label_length))]
    pub low: Option<String>,
    /// Label under the middle point, e.g. "Neutral".
    #[garde(length(chars, max = ctx.scale.max_label_length))]
    pub mid: Option<String>,
    /// Label under the highest point, e.g. "Strongly agree".
    #[garde(length(chars, max = ctx.scale.max_label_length))]
    pub high: Option<String>,
}

/// Configuration for a scale slide
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
    /// Time before the scale is displayed.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can pick a rating.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Points awarded. Scales collect opinions, so this is `0` in practice; the
    /// field exists so every slide type announces its scoring the same way.
    #[garde(skip)]
    #[serde(default)]
    points_awarded: u64,
    /// Lowest point on the scale
    #[garde(skip)]
    #[serde(default = "default_min")]
    min: i64,
    /// Highest point on the scale
    #[garde(custom(|val: &i64, ctx: &crate::settings::Settings| validate_max(*val, ctx)))]
    #[serde(default = "default_max")]
    max: i64,
    /// Which flavour of scale to present
    #[garde(skip)]
    #[serde(default)]
    style: Style,
    /// Labels shown beneath the scale
    #[garde(dive)]
    #[serde(default)]
    labels: Labels,
}

fn default_min() -> i64 {
    1
}

fn default_max() -> i64 {
    5
}

fn validate_max(max: i64, ctx: &crate::settings::Settings) -> garde::Result {
    // `min` isn't reachable from a per-field validator, so bound `max` on its
    // own and let `SlideConfig::points` reject an inverted pair at runtime by
    // yielding an empty scale.
    let allowed = i64::try_from(ctx.scale.max_points_count).unwrap_or(i64::MAX);
    if max > allowed {
        return Err(garde::Error::new(format!(
            "scale maximum {max} exceeds the {allowed} allowed points"
        )));
    }
    Ok(())
}

/// Runtime state for a scale slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Player ratings (already clamped into range) with submission timestamps
    user_answers: FxHashMap<Id, (i64, Timestamp)>,
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

    /// Every selectable point, ascending. Empty when `max < min`.
    fn points(&self) -> Vec<i64> {
        (self.min..=self.max).collect()
    }
}

/// Net Promoter Score breakdown, present only on [`Style::Nps`] slides.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct NpsBreakdown {
    /// Ratings of 9 or 10.
    pub promoters: usize,
    /// Ratings of 7 or 8.
    pub passives: usize,
    /// Ratings of 6 or below.
    pub detractors: usize,
    /// `%promoters - %detractors`, in the conventional -100..100 range.
    pub score: f64,
}

/// Aggregated results for a scale slide.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    /// One count per selectable point, ascending and aligned with the scale.
    pub counts: Vec<usize>,
    /// Mean rating, or `None` when nobody answered.
    pub average: Option<f64>,
    /// Number of players who answered.
    pub total_count: usize,
    /// NPS breakdown, present only for [`Style::Nps`].
    pub nps: Option<NpsBreakdown>,
}

/// Messages sent to listeners to update their pre-existing scale state.
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
        /// Which flavour of scale is coming up
        style: Style,
    },
    /// Announces the question without the scale
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// Optional media content accompanying the question
        media: Option<&'a Media>,
        /// Time before the scale appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Reveals the scale and opens the answering window
    AnswersAnnouncement {
        /// Time before the answering window closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Every selectable point, ascending
        points: Vec<i64>,
        /// Labels shown beneath the scale
        labels: &'a Labels,
        /// Which flavour of scale this is
        style: Style,
    },
    /// (HOST ONLY) Reports the number of players who have submitted ratings
    AnswersCount(usize),
    /// Shows the distribution of ratings
    AnswersResults {
        /// Every selectable point, ascending
        points: Vec<i64>,
        /// Labels shown beneath the scale
        labels: &'a Labels,
        /// Which flavour of scale this is
        style: Style,
        /// Aggregated player ratings
        results: Results,
    },
}

/// Scheduled phase-transition alarm for scale slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing scale state.
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
        /// Which flavour of scale is coming up
        style: Style,
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
        /// Remaining time before the scale appears, or `None` for host-paced
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
        /// Every selectable point, ascending
        points: Vec<i64>,
        /// Labels shown beneath the scale
        labels: &'a Labels,
        /// Which flavour of scale this is
        style: Style,
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
        /// Every selectable point, ascending
        points: Vec<i64>,
        /// Labels shown beneath the scale
        labels: &'a Labels,
        /// Which flavour of scale this is
        style: Style,
        /// Aggregated player ratings
        results: Results,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<i64> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (i64, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (i64, Timestamp)> {
        &mut self.user_answers
    }

    fn transform_answer(&self, answer: i64) -> i64 {
        answer.clamp(self.config.min, self.config.max.max(self.config.min))
    }

    /// Opinions have no right answer, so nothing scores.
    fn is_correct_answer(&self, _answer: &i64) -> bool {
        false
    }

    fn describe_answer(&self, answer: &i64) -> String {
        answer.to_string()
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
                    points: self.config.points(),
                    labels: &self.config.labels,
                    style: self.config.style,
                    results: self.results(),
                }
                .into(),
                tunnel_finder,
            );
        }
    }
}

impl PhasedSlide<i64> for State {
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
                        points: self.config.points(),
                        labels: &self.config.labels,
                        style: self.config.style,
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
                style: self.config.style,
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

    /// Tallies ratings into one bucket per selectable point.
    fn results(&self) -> Results {
        let points = self.config.points();
        let mut counts = vec![0_usize; points.len()];
        let mut sum = 0_i64;

        for (rating, _) in self.user_answers.values() {
            if let Some(offset) = rating.checked_sub(self.config.min)
                && let Ok(offset) = usize::try_from(offset)
                && let Some(slot) = counts.get_mut(offset)
            {
                *slot += 1;
            }
            sum += rating;
        }

        let total_count = self.user_answers.len();

        Results {
            counts,
            average: (total_count > 0).then(|| sum as f64 / total_count as f64),
            total_count,
            nps: (self.config.style == Style::Nps).then(|| self.nps_breakdown()),
        }
    }

    /// Splits ratings into the conventional NPS bands.
    fn nps_breakdown(&self) -> NpsBreakdown {
        let mut promoters = 0;
        let mut passives = 0;
        let mut detractors = 0;
        for (rating, _) in self.user_answers.values() {
            match rating {
                9..=10 => promoters += 1,
                7..=8 => passives += 1,
                _ => detractors += 1,
            }
        }
        let total = promoters + passives + detractors;
        NpsBreakdown {
            promoters,
            passives,
            detractors,
            score: if total == 0 {
                0.0
            } else {
                (promoters as f64 - detractors as f64) / total as f64 * 100.0
            },
        }
    }

    /// Starts the scale slide by entering the [`Phase::Unstarted`] phase.
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
                style: self.config.style,
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
                points: self.config.points(),
                labels: &self.config.labels,
                style: self.config.style,
                answered_count: get_answered_count(self),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                points: self.config.points(),
                labels: &self.config.labels,
                style: self.config.style,
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
        if let crate::AlarmMessage::Scale(inner) = message {
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
        if self.state() != Phase::Answers || self.config.max < self.config.min {
            return;
        }
        if let IncomingPlayerMessage::NumberAnswer(value) = message
            && value.is_finite()
        {
            self.record_answer_at(watcher_id, value.round() as i64, tick.now());
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn config(min: i64, max: i64, style: Style) -> SlideConfig {
        SlideConfig {
            title: "How was it?".to_string(),
            media: None,
            introduce_slide: None,
            introduce_question: None,
            time_limit: None,
            points_awarded: 0,
            min,
            max,
            style,
            labels: Labels::default(),
        }
    }

    #[test]
    fn points_span_the_configured_range() {
        assert_eq!(config(1, 5, Style::Agreement).points(), vec![1, 2, 3, 4, 5]);
        assert_eq!(config(0, 10, Style::Nps).points().len(), 11);
        assert_eq!(config(5, 1, Style::Agreement).points(), [] as [i64; 0]);
    }

    #[test]
    fn ratings_are_clamped_into_range() {
        let mut state = config(1, 5, Style::Agreement).to_state();
        let low = Id::new();
        let high = Id::new();
        state.record_answer(low, -3);
        state.record_answer(high, 99);
        assert_eq!(state.user_answers().get(&low).map(|(v, _)| *v), Some(1));
        assert_eq!(state.user_answers().get(&high).map(|(v, _)| *v), Some(5));
    }

    #[test]
    fn counts_align_with_the_scale_points() {
        let mut state = config(1, 5, Style::Agreement).to_state();
        state.record_answer(Id::new(), 1);
        state.record_answer(Id::new(), 3);
        state.record_answer(Id::new(), 3);
        state.record_answer(Id::new(), 5);

        let results = state.results();
        assert_eq!(results.counts, vec![1, 0, 2, 0, 1]);
        assert_eq!(results.total_count, 4);
        assert!((results.average.expect("answers exist") - 3.0).abs() < 1e-9);
        assert!(results.nps.is_none(), "agreement scales carry no NPS breakdown");
    }

    #[test]
    fn nps_splits_into_promoters_passives_and_detractors() {
        let mut state = config(0, 10, Style::Nps).to_state();
        for rating in [10, 9, 8, 7, 6, 0] {
            state.record_answer(Id::new(), rating);
        }

        let nps = state.results().nps.expect("nps style reports a breakdown");
        assert_eq!(nps.promoters, 2);
        assert_eq!(nps.passives, 2);
        assert_eq!(nps.detractors, 2);
        assert!(nps.score.abs() < 1e-9, "equal promoters and detractors score 0");
    }

    #[test]
    fn nps_score_is_zero_without_answers() {
        let state = config(0, 10, Style::Nps).to_state();
        let nps = state.results().nps.expect("nps style reports a breakdown");
        assert_eq!(nps.promoters + nps.passives + nps.detractors, 0);
        assert!(nps.score.abs() < f64::EPSILON);
    }

    #[test]
    fn opinions_never_score() {
        let state = config(1, 5, Style::Agreement).to_state();
        assert!(!state.is_correct_answer(&5));
        assert!(state.score_multiplier(&5).abs() < f64::EPSILON);
    }
}
