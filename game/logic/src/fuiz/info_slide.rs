//! Info slide (present information) implementation
//!
//! An info slide asks nothing. It shows a heading, an optional body and an
//! optional image, and then either advances on its own after `duration` or
//! waits for the host. It exists so a quiz can carry teaching material between
//! questions without dropping out of the game.
//!
//! Nothing is scored, but the slide still records a zero for every player when
//! it ends so per-slide point arrays stay aligned with the slide indices.

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
    watcher::{Id, Watchers},
};

use super::{
    super::game::IncomingPlayerMessage,
    common::{
        AnswerHandler, PhasedSlide, ProceedFromSlideIntoSlide, QuestionReceiveMessage, SlideCore, SlideStateManager,
        SlideTimer, add_scores_to_leaderboard, impl_slide_core,
    },
    media::Media,
};

/// Lifecycle phases for an info slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has been shown.
    #[default]
    Unstarted,
    /// The slide is on screen. Terminal: the next step leaves the slide.
    Content,
}

impl super::common::Phase for Phase {
    fn next(self) -> Option<Self> {
        match self {
            Self::Unstarted => Some(Self::Content),
            Self::Content => None,
        }
    }
}

/// Configuration for an info slide
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The heading shown at the top of the slide
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Body copy shown under the heading
    #[garde(length(chars, max = ctx.info_slide.max_body_length))]
    #[serde(default)]
    body: Option<String>,
    /// Accompanying media
    #[garde(dive)]
    media: Option<Media>,
    /// How long the slide stays up before the game moves on.
    /// `None` means host-paced: the slide waits for the host.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    duration: Option<Duration>,
}

/// Runtime state for an info slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// Always empty: an info slide takes no answers. Present so the slide can
    /// reuse the shared scoring path, which records a zero for every player.
    user_answers: FxHashMap<Id, ((), Timestamp)>,
    /// Shared runtime core: slide phase and the on-screen timer.
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

/// Messages sent to listeners to update their pre-existing info-slide state.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Puts the slide on screen
    ContentAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The heading shown at the top of the slide
        title: &'a str,
        /// Body copy shown under the heading
        body: Option<&'a str>,
        /// Accompanying media
        media: Option<&'a Media>,
        /// Time before the game moves on, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
}

/// Scheduled advance alarm for info slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing info-slide state.
///
/// See [`UpdateMessage`] for an explanation of these fields.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage<'a> {
    /// Synchronizes the on-screen slide
    ContentAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The heading shown at the top of the slide
        title: &'a str,
        /// Body copy shown under the heading
        body: Option<&'a str>,
        /// Accompanying media
        media: Option<&'a Media>,
        /// Remaining time before the game moves on, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<()> for State {
    fn user_answers(&self) -> &FxHashMap<Id, ((), Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, ((), Timestamp)> {
        &mut self.user_answers
    }

    /// There is nothing to get right.
    fn is_correct_answer(&self, (): &()) -> bool {
        false
    }

    fn describe_answer(&self, _answer: &()) -> String {
        // Nothing is asked, so nothing is answered.
        String::new()
    }

    fn max_points(&self) -> u64 {
        0
    }

    fn time_limit(&self) -> Option<Duration> {
        None
    }

    fn answers_count_message(_count: usize) -> crate::UpdateMessage<'static> {
        // Never sent: an info slide has no answers to count. The variant chosen
        // here only has to type-check.
        UpdateMessage::ContentAnnouncement {
            index: 0,
            count: 0,
            title: "",
            body: None,
            media: None,
            duration: None,
        }
        .into()
    }

    /// An info slide has no results screen; leaving it advances the game.
    fn send_answers_results<F: TunnelFinder>(&mut self, _watchers: &Watchers, _tunnel_finder: F) {}
}

impl PhasedSlide<()> for State {
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
            Phase::Unstarted => {}
            Phase::Content => {
                if !self.change_state(Phase::Unstarted, Phase::Content) {
                    return;
                }
                self.start_timer_at(tick.now());

                watchers.announce(
                    &UpdateMessage::ContentAnnouncement {
                        index,
                        count,
                        title: &self.config.title,
                        body: self.config.body.as_deref(),
                        media: self.config.media.as_ref(),
                        duration: self.config.duration,
                    }
                    .into(),
                    tunnel_finder,
                );

                if let Some(duration) = self.config.duration {
                    schedule_message(
                        AlarmMessage {
                            index,
                            to: Phase::Content,
                        }
                        .into(),
                        duration,
                    );
                }
            }
        }
    }
}

impl State {
    /// Puts the slide on screen.
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
            Phase::Content,
            None,
            watchers,
            schedule_message,
            tick,
            tunnel_finder,
            index,
            count,
        );
    }

    /// How long the slide stays up on its own, or `None` if it waits for the
    /// host.
    pub(crate) fn duration(&self) -> Option<Duration> {
        self.config.duration
    }

    /// Synchronization message for a newly connected watcher.
    pub fn state_message(&self, index: usize, count: usize) -> SyncMessage<'_> {
        SyncMessage::ContentAnnouncement {
            index,
            count,
            title: &self.config.title,
            body: self.config.body.as_deref(),
            media: self.config.media.as_ref(),
            duration: match self.state() {
                // Not on screen yet, so the full duration is still ahead.
                Phase::Unstarted => self.config.duration,
                Phase::Content => self.config.duration.map(|d| d.saturating_sub(self.elapsed())),
            },
        }
    }

    /// The slide's timer firing ends the slide rather than changing its phase,
    /// so this scores the (empty) slide and tells the game to move on.
    pub(crate) fn receive_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        message: &crate::AlarmMessage,
    ) -> SlideAction<S> {
        if !matches!(message, crate::AlarmMessage::InfoSlide(_)) || self.state() != Phase::Content {
            return SlideAction::Stay;
        }
        add_scores_to_leaderboard(self, leaderboard, watchers, team_manager, tick.now(), &tunnel_finder);
        SlideAction::Next { schedule_message, tick }
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

    /// Info slides ignore players entirely.
    fn receive_player_message<F: TunnelFinder>(
        &mut self,
        _watcher_id: Id,
        _message: IncomingPlayerMessage,
        _watchers: &Watchers,
        _tick: Tick,
        _tunnel_finder: F,
    ) {
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::fuiz::common::Phase as _;

    fn config(duration: Option<Duration>) -> SlideConfig {
        SlideConfig {
            title: "A word on photosynthesis".to_string(),
            body: Some("Plants turn light into sugar.".to_string()),
            media: None,
            duration,
        }
    }

    #[test]
    fn content_is_the_terminal_phase() {
        assert_eq!(Phase::Unstarted.next(), Some(Phase::Content));
        assert_eq!(Phase::Content.next(), None);
    }

    #[test]
    fn sync_before_display_offers_the_full_duration() {
        let state = config(Some(Duration::from_secs(20))).to_state();
        let SyncMessage::ContentAnnouncement {
            duration, title, body, ..
        } = state.state_message(0, 1);
        assert_eq!(duration, Some(Duration::from_secs(20)));
        assert_eq!(title, "A word on photosynthesis");
        assert_eq!(body, Some("Plants turn light into sugar."));
    }

    #[test]
    fn host_paced_slides_carry_no_duration() {
        let state = config(None).to_state();
        let SyncMessage::ContentAnnouncement { duration, .. } = state.state_message(0, 1);
        assert!(duration.is_none());
    }

    #[test]
    fn nothing_scores() {
        let state = config(None).to_state();
        assert_eq!(state.max_points(), 0);
        assert!(!state.is_correct_answer(&()));
        assert!(state.user_answers().is_empty());
    }
}
