//! Pin (drop a marker on an image) question implementation
//!
//! Players tap a point on the slide's image. Two flavours share this module
//! because the mechanic is identical and only the scoring differs:
//!
//! - **Pin answer**: `correct_area` is set, and a pin inside it earns points.
//! - **Drop pin**: `correct_area` is absent, so the slide simply collects
//!   opinions and the room sees where everyone pinned.
//!
//! Coordinates are normalised to `0.0..=1.0` of the image's width and height,
//! so they survive any rendering size. Values arriving outside that box are
//! clamped rather than rejected; a pin dropped a pixel off the edge is still
//! a pin.

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

/// Lifecycle phases for a pin slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the question without the pinnable image.
    Question,
    /// Showing the image and accepting pins.
    Answers,
    /// Displaying every pin, and the target when there is one.
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

/// A point on the image, normalised to `0.0..=1.0` on both axes.
///
/// The origin is the image's top-left corner, matching how the client reports
/// a tap.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Point {
    /// Horizontal position as a fraction of the image width.
    pub x: f64,
    /// Vertical position as a fraction of the image height.
    pub y: f64,
}

impl Point {
    /// Pulls a point back inside the image.
    ///
    /// Infinities clamp to the nearest edge like any other out-of-range value;
    /// only NaN, which `clamp` would propagate, falls back to the centre.
    fn clamped(self) -> Self {
        fn into_image(value: f64) -> f64 {
            if value.is_nan() { 0.5 } else { value.clamp(0.0, 1.0) }
        }
        Self {
            x: into_image(self.x),
            y: into_image(self.y),
        }
    }
}

/// The region that earns points on a pin-answer slide.
///
/// Every coordinate is normalised to `0.0..=1.0` of the image, so a shape means
/// the same thing on a phone and a projector. Because widths and heights are
/// carried independently there is no aspect ratio to reconcile: an ellipse
/// drawn as a circle over a wide photo stays a circle when it's redrawn.
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub enum Shape {
    /// An axis-aligned box, anchored at its top-left corner.
    Rectangle {
        /// Left edge.
        #[garde(skip)]
        x: f64,
        /// Top edge.
        #[garde(skip)]
        y: f64,
        /// Width as a fraction of the image width.
        #[garde(skip)]
        width: f64,
        /// Height as a fraction of the image height.
        #[garde(skip)]
        height: f64,
    },
    /// An axis-aligned ellipse. Equal radii on a square image give a circle.
    Ellipse {
        /// Centre of the ellipse.
        #[garde(skip)]
        center: Point,
        /// Horizontal radius as a fraction of the image width.
        #[garde(skip)]
        radius_x: f64,
        /// Vertical radius as a fraction of the image height.
        #[garde(skip)]
        radius_y: f64,
    },
    /// A freehand outline, implicitly closed from the last point to the first.
    Polygon {
        /// The traced outline, in order.
        #[garde(length(min = 3, max = ctx.pin.max_polygon_points))]
        points: Vec<Point>,
    },
}

impl Shape {
    /// True when `point` falls inside the region.
    ///
    /// A degenerate shape (zero width, a collapsed outline) contains nothing,
    /// which is the safe reading: nobody scores rather than everybody.
    fn contains(&self, point: Point) -> bool {
        match self {
            Self::Rectangle { x, y, width, height } => {
                let (left, right) = ordered(*x, x + width);
                let (top, bottom) = ordered(*y, y + height);
                point.x >= left && point.x <= right && point.y >= top && point.y <= bottom
            }
            Self::Ellipse {
                center,
                radius_x,
                radius_y,
            } => {
                if !(radius_x.is_finite() && radius_y.is_finite()) || *radius_x <= 0.0 || *radius_y <= 0.0 {
                    return false;
                }
                let dx = (point.x - center.x) / radius_x;
                let dy = (point.y - center.y) / radius_y;
                dx * dx + dy * dy <= 1.0
            }
            Self::Polygon { points } => contains_in_polygon(points, point),
        }
    }
}

/// Sorts a pair so a shape drawn right-to-left or bottom-to-top still describes
/// the box the author dragged out.
fn ordered(a: f64, b: f64) -> (f64, f64) {
    if a <= b { (a, b) } else { (b, a) }
}

/// Even-odd ray casting: counts how many edges a ray to the right crosses.
fn contains_in_polygon(points: &[Point], probe: Point) -> bool {
    if points.len() < 3 {
        return false;
    }
    let mut inside = false;
    let mut previous = points[points.len() - 1];
    for current in points {
        // Only edges that straddle the probe's row can be crossed.
        if (current.y > probe.y) != (previous.y > probe.y) {
            let span = previous.y - current.y;
            if span != 0.0 {
                let crossing_x = (previous.x - current.x) * (probe.y - current.y) / span + current.x;
                if probe.x < crossing_x {
                    inside = !inside;
                }
            }
        }
        previous = *current;
    }
    inside
}

/// Configuration for a pin slide
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The question title, represents what's being asked
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// The image players pin on. Without it there is nothing to aim at, so the
    /// client always supplies one.
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
    /// Time before the image becomes pinnable.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can drop their pin.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    time_limit: Option<Duration>,
    /// Maximum number of points awarded, decreasing linearly to half by the end
    /// of the slide. Drop-pin slides collect opinions and use `0`.
    #[garde(skip)]
    #[serde(default)]
    points_awarded: u64,
    /// The region that earns points. `None` turns this into a drop pin: every
    /// placement is equally valid and nothing scores.
    #[garde(dive)]
    #[serde(default)]
    correct_area: Option<Shape>,
}

/// Runtime state for a pin slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Player pins (already clamped into the image) with submission timestamps
    user_answers: FxHashMap<Id, (Point, Timestamp)>,
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

/// Aggregated results for a pin slide.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    /// Individual pins, capped at the configured reporting limit so a very
    /// large room doesn't ship a megabyte of coordinates.
    pub pins: Vec<Point>,
    /// Number of players who pinned inside the target, or `None` for drop pins.
    pub correct_count: Option<usize>,
    /// Number of players who pinned at all.
    pub total_count: usize,
}

/// Messages sent to listeners to update their pre-existing pin state.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Announces the upcoming question's type and scoring (the `Unstarted` phase).
    SlideAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// Maximum points awarded for a pin inside the target
        points_awarded: u64,
        /// Duration of the intro, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Whether this slide has a target to aim at
        scored: bool,
    },
    /// Announces the question without the pinnable image
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// The image players will pin on
        media: Option<&'a Media>,
        /// Time before pinning opens, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Opens the pinning window
    AnswersAnnouncement {
        /// Time before pinning closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Whether this slide has a target to aim at
        scored: bool,
    },
    /// (HOST ONLY) Reports the number of players who have pinned
    AnswersCount(usize),
    /// Shows every pin, and the target when there is one
    AnswersResults {
        /// The region that earned points, or `None` for drop pins
        correct_area: Option<&'a Shape>,
        /// Every player pin, for the host's map. Absent for everyone else,
        /// whose screen shows only their own pin against the target.
        results: Option<Results>,
    },
}

/// Scheduled phase-transition alarm for pin slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing pin state.
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
        /// Maximum points awarded for a pin inside the target
        points_awarded: u64,
        /// Remaining intro time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Whether this slide has a target to aim at
        scored: bool,
    },
    /// Synchronizes the question announcement phase
    QuestionAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// The image players will pin on
        media: Option<&'a Media>,
        /// Remaining time before pinning opens, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Synchronizes the pinning phase
    AnswersAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The question text being asked
        question: &'a str,
        /// The image players pin on
        media: Option<&'a Media>,
        /// Remaining pinning time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// Whether this slide has a target to aim at
        scored: bool,
        /// Number of players who have already pinned
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
        /// The image players pinned on
        media: Option<&'a Media>,
        /// The region that earned points, or `None` for drop pins
        correct_area: Option<&'a Shape>,
        /// Every player pin, for the host's map. Absent for everyone else,
        /// whose screen shows only their own pin against the target.
        results: Option<Results>,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<Point> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (Point, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (Point, Timestamp)> {
        &mut self.user_answers
    }

    fn transform_answer(&self, answer: Point) -> Point {
        answer.clamped()
    }

    fn is_correct_answer(&self, answer: &Point) -> bool {
        self.config
            .correct_area
            .as_ref()
            .is_some_and(|area| area.contains(*answer))
    }

    fn recap_answer(&self, _answer: &Point) -> Option<String> {
        // Coordinates mean nothing without the picture under them.
        None
    }

    fn describe_answer(&self, answer: &Point) -> String {
        // A coordinate pair is the only honest way to name a spot on a picture.
        format!("{:.0}%, {:.0}%", answer.x * 100.0, answer.y * 100.0)
    }

    fn question_text(&self) -> &str {
        &self.config.title
    }

    fn is_graded(&self) -> bool {
        // A target makes a pin question; without one it is a drop pin, an opinion.
        self.config.correct_area.is_some()
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
                            correct_area: self.config.correct_area.as_ref(),
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

impl PhasedSlide<Point> for State {
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
                        scored: self.config.correct_area.is_some(),
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
    /// phase), then auto-advances after `introduce_slide`.
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
                scored: self.config.correct_area.is_some(),
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

    /// Collects every pin. Sent to the host alone, so the room does not each
    /// carry a copy of a payload only the host's map draws.
    fn results(&self) -> Results {
        Results {
            pins: self.user_answers.values().map(|(point, _)| *point).collect_vec(),
            correct_count: self.config.correct_area.as_ref().map(|_| self.correct_count()),
            total_count: self.user_answers.len(),
        }
    }

    /// Starts the pin slide by entering the [`Phase::Unstarted`] phase.
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
                scored: self.config.correct_area.is_some(),
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
                scored: self.config.correct_area.is_some(),
                answered_count: get_answered_count(self),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                correct_area: self.config.correct_area.as_ref(),
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
        if let crate::AlarmMessage::Pin(inner) = message {
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
        if let IncomingPlayerMessage::PointAnswer { x, y } = message {
            self.record_answer_at(watcher_id, Point { x, y }, tick.now());
            self.handle_post_answer(watchers, &tunnel_finder);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    /// Pin coordinates ride through float arithmetic, so compare them by
    /// closeness rather than by bit pattern.
    #[track_caller]
    fn assert_coord(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-9, "expected {expected}, got {actual}");
    }

    fn ellipse(x: f64, y: f64, rx: f64, ry: f64) -> Shape {
        Shape::Ellipse {
            center: Point { x, y },
            radius_x: rx,
            radius_y: ry,
        }
    }

    #[test]
    fn the_recap_does_not_quote_coordinates() {
        let state = config(Some(ellipse(0.5, 0.5, 0.1, 0.1))).to_state();
        assert_eq!(state.recap_answer(&Point { x: 0.61, y: 0.38 }), None);
    }

    fn config(correct_area: Option<Shape>) -> SlideConfig {
        SlideConfig {
            title: "Where is Rome?".to_string(),
            media: None,
            introduce_slide: None,
            introduce_question: None,
            time_limit: None,
            points_awarded: if correct_area.is_some() { 1000 } else { 0 },
            correct_area,
        }
    }

    #[test]
    fn pins_are_clamped_into_the_image() {
        let mut state = config(None).to_state();
        let id = Id::new();
        state.record_answer(id, Point { x: -0.5, y: 2.0 });
        let stored = state.user_answers().get(&id).map(|(p, _)| *p).expect("recorded");
        assert_coord(stored.x, 0.0);
        assert_coord(stored.y, 1.0);
    }

    #[test]
    fn nan_falls_back_to_the_centre_while_infinity_clamps() {
        let mut state = config(None).to_state();
        let id = Id::new();
        state.record_answer(
            id,
            Point {
                x: f64::NAN,
                y: f64::INFINITY,
            },
        );
        let stored = state.user_answers().get(&id).map(|(p, _)| *p).expect("recorded");
        assert_coord(stored.x, 0.5);
        assert_coord(stored.y, 1.0);
    }

    #[test]
    fn an_ellipse_uses_independent_radii() {
        // Drawn over a wide image: half as tall as it is wide in normalised
        // space, which is a circle on screen.
        let shape = ellipse(0.5, 0.5, 0.2, 0.1);
        assert!(shape.contains(Point { x: 0.68, y: 0.5 }));
        assert!(shape.contains(Point { x: 0.5, y: 0.58 }));
        assert!(!shape.contains(Point { x: 0.5, y: 0.65 }));
        assert!(!shape.contains(Point { x: 0.72, y: 0.5 }));
    }

    #[test]
    fn a_rectangle_covers_its_box() {
        let shape = Shape::Rectangle {
            x: 0.2,
            y: 0.3,
            width: 0.4,
            height: 0.2,
        };
        assert!(shape.contains(Point { x: 0.2, y: 0.3 }));
        assert!(shape.contains(Point { x: 0.6, y: 0.5 }));
        assert!(shape.contains(Point { x: 0.4, y: 0.4 }));
        assert!(!shape.contains(Point { x: 0.19, y: 0.4 }));
        assert!(!shape.contains(Point { x: 0.4, y: 0.51 }));
    }

    #[test]
    fn a_rectangle_dragged_backwards_still_covers_its_box() {
        // Dragging up and to the left gives negative width and height.
        let shape = Shape::Rectangle {
            x: 0.6,
            y: 0.5,
            width: -0.4,
            height: -0.2,
        };
        assert!(shape.contains(Point { x: 0.4, y: 0.4 }));
        assert!(!shape.contains(Point { x: 0.1, y: 0.4 }));
    }

    #[test]
    fn a_polygon_uses_its_outline() {
        // A triangle over the top-left half of the image.
        let shape = Shape::Polygon {
            points: vec![
                Point { x: 0.0, y: 0.0 },
                Point { x: 1.0, y: 0.0 },
                Point { x: 0.0, y: 1.0 },
            ],
        };
        assert!(shape.contains(Point { x: 0.2, y: 0.2 }));
        assert!(!shape.contains(Point { x: 0.8, y: 0.8 }));
    }

    #[test]
    fn a_concave_polygon_excludes_its_notch() {
        // An arrowhead: the notch between the barbs is outside the shape.
        let shape = Shape::Polygon {
            points: vec![
                Point { x: 0.5, y: 0.0 },
                Point { x: 1.0, y: 1.0 },
                Point { x: 0.5, y: 0.7 },
                Point { x: 0.0, y: 1.0 },
            ],
        };
        assert!(shape.contains(Point { x: 0.5, y: 0.4 }));
        assert!(!shape.contains(Point { x: 0.5, y: 0.9 }), "the notch is outside");
    }

    #[test]
    fn degenerate_shapes_contain_nothing() {
        assert!(!ellipse(0.5, 0.5, 0.0, 0.2).contains(Point { x: 0.5, y: 0.5 }));
        assert!(
            !Shape::Polygon {
                points: vec![Point { x: 0.0, y: 0.0 }, Point { x: 1.0, y: 1.0 }]
            }
            .contains(Point { x: 0.5, y: 0.5 })
        );
        assert!(
            !ellipse(0.5, 0.5, f64::NAN, 0.2).contains(Point { x: 0.5, y: 0.5 }),
            "a non-finite radius must not swallow the image"
        );
    }

    #[test]
    fn drop_pins_never_score() {
        let state = config(None).to_state();
        assert!(!state.is_correct_answer(&Point { x: 0.5, y: 0.5 }));
        assert!(state.results().correct_count.is_none());
    }

    #[test]
    fn results_count_hits_against_the_target() {
        let mut state = config(Some(ellipse(0.5, 0.5, 0.1, 0.1))).to_state();
        state.record_answer(Id::new(), Point { x: 0.5, y: 0.5 });
        state.record_answer(Id::new(), Point { x: 0.52, y: 0.5 });
        state.record_answer(Id::new(), Point { x: 0.9, y: 0.9 });

        let results = state.results();
        assert_eq!(results.total_count, 3);
        assert_eq!(results.correct_count, Some(2));
        assert_eq!(results.pins.len(), 3);
    }
}
