//! Fuiz configuration and question management
//!
//! This module defines the core configuration structures for Fuiz games,
//! including the main `Fuiz` struct, slide configurations, and the runtime
//! state management for different question types. It provides the central
//! coordination layer that manages question flow and state transitions.

use garde::Validate;
use serde::{Deserialize, Serialize};

use super::{
    super::game::IncomingMessage, brainstorm, free_text, info_slide, media::Media, multiple_choice, order, pin, poll,
    scale, slider, type_answer,
};
use crate::fuiz::common::QuestionReceiveMessage;
use crate::tick::Tick;
use crate::{
    AlarmMessage, SyncMessage,
    leaderboard::Leaderboard,
    session::TunnelFinder,
    teams::TeamManager,
    watcher::{Id, ValueKind, Watchers},
};

/// Alias for a function that schedules alarm messages
pub trait ScheduleMessageFn: FnOnce(AlarmMessage, std::time::Duration) {}

impl<T: FnOnce(AlarmMessage, std::time::Duration)> ScheduleMessageFn for T {}

/// Represents owned content that can be either text or media.
///
/// This is the storage form, used inside [`crate::fuiz::config::Fuiz`] and
/// the slide configs. Outgoing messages carry the borrowed counterpart
/// [`TextOrMediaRef`] to avoid cloning the underlying strings per recipient.
#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub enum TextOrMedia {
    /// Media content (images, etc.)
    Media(#[garde(skip)] Media),
    /// Plain text content with length validation
    Text(#[garde(length(max = ctx.answer_text.max_length))] String),
}

/// Borrowed view of [`TextOrMedia`] for outgoing messages.
///
/// Constructed via [`TextOrMedia::as_ref`]. Serialises identically to
/// [`TextOrMedia`] (the variant tags and contents are the same), but holds
/// references into the slide config rather than owning copies.
#[derive(Debug, Serialize, Clone, Copy)]
pub enum TextOrMediaRef<'a> {
    /// Media content (images, etc.)
    Media(&'a Media),
    /// Plain text content
    Text(&'a str),
}

impl TextOrMedia {
    /// Returns a borrowed view of this value for use in outgoing messages.
    pub fn as_ref(&self) -> TextOrMediaRef<'_> {
        match self {
            Self::Media(m) => TextOrMediaRef::Media(m),
            Self::Text(t) => TextOrMediaRef::Text(t),
        }
    }
}

/// A complete Fuiz configuration containing all questions and settings
///
/// This is the main configuration structure that defines an entire quiz game,
/// including the title and all slides/questions that will be presented to players.
#[derive(Debug, Clone, Serialize, Deserialize, Default, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct Fuiz {
    /// The title of the Fuiz game (currently unused in gameplay)
    #[garde(length(max = ctx.fuiz.max_title_length))]
    pub title: String,

    /// The collection of slides/questions in the game
    #[garde(length(max = ctx.fuiz.max_slides_count), dive)]
    pub slides: Vec<SlideConfig>,
}

/// Represents a currently active slide with its runtime state
///
/// This struct tracks which slide is currently being presented and
/// maintains its runtime state for player interactions and timing.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct CurrentSlide {
    /// The index of the current slide in the slides vector
    pub index: usize,
    /// The runtime state of the current slide
    pub state: SlideState,
}

/// Configuration for a single slide/question
///
/// This enum represents the different types of questions that can be
/// included in a Fuiz game. Each variant contains the specific configuration
/// for that question type, including timing, content, and scoring parameters.
#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub enum SlideConfig {
    /// A multiple choice question with predefined answer options
    MultipleChoice(#[garde(dive)] multiple_choice::SlideConfig),
    /// A type answer question where players enter free text
    TypeAnswer(#[garde(dive)] type_answer::SlideConfig),
    /// An order question where players arrange items in sequence
    Order(#[garde(dive)] order::SlideConfig),
    /// A slider question where players estimate a value on a numeric range
    Slider(#[garde(dive)] slider::SlideConfig),
    /// A scale question where players rate on an agreement or NPS scale
    Scale(#[garde(dive)] scale::SlideConfig),
    /// A poll where players vote between options with no right answer
    Poll(#[garde(dive)] poll::SlideConfig),
    /// A pin question where players drop a marker on an image
    Pin(#[garde(dive)] pin::SlideConfig),
    /// A free-text question collecting a word cloud or open-ended responses
    FreeText(#[garde(dive)] free_text::SlideConfig),
    /// A brainstorm where players contribute ideas and then vote on them
    Brainstorm(#[garde(dive)] brainstorm::SlideConfig),
    /// An info slide that presents content without asking anything
    InfoSlide(#[garde(dive)] info_slide::SlideConfig),
}

impl SlideConfig {
    /// Converts this configuration into a runtime state
    ///
    /// This method creates the initial runtime state for a slide based on
    /// its configuration, preparing it for active gameplay.
    ///
    /// # Returns
    ///
    /// A new `SlideState` initialized from this configuration
    pub fn to_state(&self) -> SlideState {
        match self {
            Self::MultipleChoice(s) => SlideState::MultipleChoice(s.to_state()),
            Self::TypeAnswer(s) => SlideState::TypeAnswer(s.to_state()),
            Self::Order(s) => SlideState::Order(s.to_state()),
            Self::Slider(s) => SlideState::Slider(s.to_state()),
            Self::Scale(s) => SlideState::Scale(s.to_state()),
            Self::Poll(s) => SlideState::Poll(s.to_state()),
            Self::Pin(s) => SlideState::Pin(s.to_state()),
            Self::FreeText(s) => SlideState::FreeText(s.to_state()),
            Self::Brainstorm(s) => SlideState::Brainstorm(s.to_state()),
            Self::InfoSlide(s) => SlideState::InfoSlide(s.to_state()),
        }
    }
}

/// Runtime state for a slide during active gameplay
///
/// This enum represents the active state of a slide while it's being
/// presented to players. It maintains timing information, player responses,
/// and current phase information for each question type.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub enum SlideState {
    /// Runtime state for a multiple choice question
    MultipleChoice(multiple_choice::State),
    /// Runtime state for a type answer question
    TypeAnswer(type_answer::State),
    /// Runtime state for an order question
    Order(order::State),
    /// Runtime state for a slider question
    Slider(slider::State),
    /// Runtime state for a scale question
    Scale(scale::State),
    /// Runtime state for a poll
    Poll(poll::State),
    /// Runtime state for a pin question
    Pin(pin::State),
    /// Runtime state for a free-text question
    FreeText(free_text::State),
    /// Runtime state for a brainstorm
    Brainstorm(brainstorm::State),
    /// Runtime state for an info slide
    InfoSlide(info_slide::State),
}

/// Runs the same expression against whichever slide type `$slide` holds.
///
/// Most of [`SlideState`]'s methods forward an identical call to every variant;
/// spelling out ten arms per method buries the one line that matters.
macro_rules! dispatch_slide {
    ($slide:expr, $inner:ident => $call:expr) => {
        match $slide {
            SlideState::MultipleChoice($inner) => $call,
            SlideState::TypeAnswer($inner) => $call,
            SlideState::Order($inner) => $call,
            SlideState::Slider($inner) => $call,
            SlideState::Scale($inner) => $call,
            SlideState::Poll($inner) => $call,
            SlideState::Pin($inner) => $call,
            SlideState::FreeText($inner) => $call,
            SlideState::Brainstorm($inner) => $call,
            SlideState::InfoSlide($inner) => $call,
        }
    };
}

impl Fuiz {
    /// Returns the number of slides in this Fuiz
    ///
    /// # Returns
    ///
    /// The total number of slides/questions in the game
    pub fn len(&self) -> usize {
        self.slides.len()
    }

    /// Checks if this Fuiz contains any slides
    ///
    /// # Returns
    ///
    /// `true` if there are no slides, `false` if there are slides
    pub fn is_empty(&self) -> bool {
        self.slides.is_empty()
    }
}

/// Action to take after processing a slide event
/// This enum indicates whether to proceed to the next slide
/// or remain on the current slide after handling an event.
pub enum SlideAction<S: ScheduleMessageFn> {
    /// Proceed to the next slide
    Next {
        /// Function to schedule timed alarm messages, returned for further scheduling
        schedule_message: S,
        /// The tick the triggering message was handled under, carried through so
        /// the slide the game advances into stamps its state from the same reading.
        tick: Tick,
    },
    /// Stay on the current slide, potentially changing its state
    Stay,
}

impl SlideState {
    /// The host-facing position (slide index + this slide's own phase) currently
    /// shown, used to tag the host's "Next" command so a stale duplicate click is
    /// ignored (see [`crate::game::HostScreen`]). Each slide type carries its own
    /// `Phase`, so slides are free to define their phases independently.
    pub(crate) fn host_position(&self, index: usize) -> crate::game::SlidePosition {
        use crate::fuiz::common::SlideStateManager;
        use crate::game::SlidePosition;
        match self {
            Self::MultipleChoice(s) => SlidePosition::MultipleChoice {
                index,
                phase: s.state(),
            },
            Self::TypeAnswer(s) => SlidePosition::TypeAnswer {
                index,
                phase: s.state(),
            },
            Self::Order(s) => SlidePosition::Order {
                index,
                phase: s.state(),
            },
            Self::Slider(s) => SlidePosition::Slider {
                index,
                phase: s.state(),
            },
            Self::Scale(s) => SlidePosition::Scale {
                index,
                phase: s.state(),
            },
            Self::Poll(s) => SlidePosition::Poll {
                index,
                phase: s.state(),
            },
            Self::Pin(s) => SlidePosition::Pin {
                index,
                phase: s.state(),
            },
            Self::FreeText(s) => SlidePosition::FreeText {
                index,
                phase: s.state(),
            },
            Self::Brainstorm(s) => SlidePosition::Brainstorm {
                index,
                phase: s.state(),
            },
            Self::InfoSlide(s) => SlidePosition::InfoSlide {
                index,
                phase: s.state(),
            },
        }
    }

    /// Whether finishing this slide is worth a standings screen.
    ///
    /// Opinion slides (poll, scale, drop pin, word cloud, open ended,
    /// brainstorm) and info slides score nothing, so the leaderboard after them
    /// would be identical to the previous one. A pin slide with a target and a
    /// slider both score, so they keep theirs.
    pub(crate) fn awards_points(&self) -> bool {
        use crate::fuiz::common::AnswerHandler;
        match self {
            Self::InfoSlide(_) => false,
            other => dispatch_slide!(other, s => s.max_points() > 0),
        }
    }

    /// Whether the slide is on its last phase, the one a "Next" leaves the
    /// slide from.
    pub(crate) fn in_last_phase(&self) -> bool {
        use crate::fuiz::common::{Phase as _, SlideStateManager};
        dispatch_slide!(self, s => s.state().next().is_none())
    }

    /// Starts playing this slide and manages its lifecycle
    ///
    /// This method initiates the slide presentation, handles timing,
    /// and coordinates with the scheduling system for timed events.
    /// It delegates to the specific implementation for each question type.
    ///
    /// # Arguments
    ///
    /// * `team_manager` - Optional team manager for team-based games
    /// * `watchers` - The watchers manager for sending messages to participants
    /// * `schedule_message` - Function to schedule timed alarm messages
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `index` - The current slide index
    /// * `count` - The total number of slides
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        match self {
            Self::MultipleChoice(s) => {
                s.play(
                    team_manager,
                    watchers,
                    schedule_message,
                    tick,
                    tunnel_finder,
                    index,
                    count,
                );
            }
            Self::TypeAnswer(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::Order(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::Slider(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::Scale(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::Poll(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::Pin(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::FreeText(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::Brainstorm(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
            Self::InfoSlide(s) => {
                s.play(watchers, schedule_message, tick, tunnel_finder, index, count);
            }
        }
    }

    /// Processes an incoming message for this slide
    ///
    /// This method handles player and host messages during slide presentation,
    /// including answer submissions, host controls, and other interactions.
    /// It delegates to the specific implementation for each question type.
    ///
    /// # Arguments
    ///
    /// * `leaderboard` - The game's leaderboard for score tracking
    /// * `watchers` - The watchers manager for participant communication
    /// * `team_manager` - Optional team manager for team-based games
    /// * `schedule_message` - Function to schedule timed alarm messages
    /// * `watcher_id` - ID of the participant sending the message
    /// * `tunnel_finder` - Function to find communication tunnels
    /// * `message` - The incoming message to process
    /// * `index` - The current slide index
    /// * `count` - The total number of slides
    ///
    /// # Returns
    ///
    /// A `SlideAction` indicating whether to stay on the current slide or advance
    pub(crate) fn receive_message<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tick: Tick,
        watcher_id: Id,
        tunnel_finder: F,
        message: IncomingMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        dispatch_slide!(self, s => s.receive_message(
            watcher_id,
            message,
            leaderboard,
            watchers,
            team_manager,
            schedule_message,
            tick,
            tunnel_finder,
            index,
            count,
        ))
    }

    /// Moves the slide along as the host's "Next" does: into its next phase,
    /// or, from the last one, off the slide.
    pub(crate) fn receive_host_next<F: TunnelFinder, S: ScheduleMessageFn>(
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
        dispatch_slide!(self, s => s.receive_host_next(
            leaderboard,
            watchers,
            team_manager,
            schedule_message,
            tick,
            tunnel_finder,
            index,
            count,
        ))
    }

    /// Generates a state synchronization message for a specific participant
    ///
    /// This method creates a sync message that allows a participant to
    /// synchronize their view with the current state of the slide.
    /// It's used when participants connect or reconnect during gameplay.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the participant requesting synchronization
    /// * `watcher_kind` - The type of participant (host, player, etc.)
    /// * `team_manager` - Optional team manager for team-based games
    /// * `tunnel_finder` - Function to find communication tunnels
    /// * `index` - The current slide index
    /// * `count` - The total number of slides
    ///
    /// # Returns
    ///
    /// A `SyncMessage` containing the current slide state information
    pub fn state_message<F: TunnelFinder>(
        &self,
        watcher_id: Id,
        watcher_kind: ValueKind,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SyncMessage<'_> {
        match self {
            Self::MultipleChoice(s) => SyncMessage::MultipleChoice(s.state_message(
                watcher_id,
                watcher_kind,
                team_manager,
                tunnel_finder,
                index,
                count,
            )),
            Self::TypeAnswer(s) => SyncMessage::TypeAnswer(s.state_message(index, count)),
            Self::Order(s) => SyncMessage::Order(s.state_message(index, count)),
            Self::Slider(s) => SyncMessage::Slider(s.state_message(index, count)),
            Self::Scale(s) => SyncMessage::Scale(s.state_message(index, count)),
            Self::Poll(s) => SyncMessage::Poll(s.state_message(index, count)),
            Self::Pin(s) => SyncMessage::Pin(s.state_message(watcher_kind, index, count)),
            Self::FreeText(s) => SyncMessage::FreeText(s.state_message(watcher_kind, index, count)),
            Self::Brainstorm(s) => SyncMessage::Brainstorm(s.state_message(index, count)),
            Self::InfoSlide(s) => SyncMessage::InfoSlide(s.state_message(index, count)),
        }
    }

    /// Processes a scheduled alarm message for this slide
    ///
    /// This method handles timed events that were previously scheduled,
    /// such as transitioning between slide phases, timing out answers,
    /// or triggering automatic state changes. It delegates to the specific
    /// implementation for each question type.
    ///
    /// # Arguments
    ///
    /// * `watchers` - The watchers manager for participant communication
    /// * `team_manager` - Optional team manager for team-based games
    /// * `schedule_message` - Function to schedule additional timed messages
    /// * `tunnel_finder` - Function to find communication tunnels
    /// * `message` - The alarm message being processed
    /// * `index` - The current slide index
    /// * `count` - The total number of slides
    ///
    /// # Returns
    ///
    /// A `SlideAction` indicating whether to stay on the current slide or advance
    pub(crate) fn receive_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        message: &AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        match self {
            Self::MultipleChoice(s) => s.receive_alarm(
                watchers,
                team_manager,
                schedule_message,
                tick,
                tunnel_finder,
                message,
                index,
                count,
            ),
            Self::TypeAnswer(s) => {
                s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count)
            }
            Self::Order(s) => s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count),
            Self::Slider(s) => s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count),
            Self::Scale(s) => s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count),
            Self::Poll(s) => s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count),
            Self::Pin(s) => s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count),
            Self::FreeText(s) => {
                s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count)
            }
            Self::Brainstorm(s) => {
                s.receive_alarm(watchers, schedule_message, tick, tunnel_finder, message, index, count)
            }
            // The info slide's timer ends the slide rather than advancing a
            // phase, so it needs the leaderboard to record its zero scores.
            Self::InfoSlide(s) => s.receive_alarm(
                leaderboard,
                watchers,
                team_manager,
                schedule_message,
                tick,
                tunnel_finder,
                message,
            ),
        }
    }

    /// Every answer on file for this slide, as display text against the id that
    /// gave it. The game turns the ids into names; it owns the name table.
    pub(crate) fn player_answers(&self) -> Vec<(crate::watcher::Id, String)> {
        use crate::fuiz::common::AnswerHandler;
        dispatch_slide!(self, s => s.player_answers())
    }

    /// This slide as the player's recap keeps it, or `None` for an info slide,
    /// which asks nothing.
    pub(crate) fn recap(&self, index: usize) -> Option<crate::recap::SlideRecap> {
        use crate::fuiz::common::AnswerHandler;
        match self {
            Self::InfoSlide(_) => None,
            other => Some(dispatch_slide!(other, s => s.recap(index))),
        }
    }

    /// Notify the active slide that a watcher has gone offline so it can
    /// keep its live-answered counter in sync.
    pub(crate) fn mark_watcher_left(&mut self, id: crate::watcher::Id) {
        use crate::fuiz::common::AnswerHandler;
        dispatch_slide!(self, s => s.mark_watcher_left(id));
    }

    /// Notify the active slide that a watcher has reconnected so it can
    /// keep its live-answered counter in sync.
    pub(crate) fn mark_watcher_returned(&mut self, id: crate::watcher::Id) {
        use crate::fuiz::common::AnswerHandler;
        dispatch_slide!(self, s => s.mark_watcher_returned(id));
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use crate::{
        game::{IncomingHostMessage, IncomingMessage},
        leaderboard::Leaderboard,
        watcher::{Id, ValueKind, Watchers},
    };

    // Mock tunnel for testing
    struct MockTunnel;
    impl crate::session::Tunnel for MockTunnel {
        fn send_message(&self, _message: &crate::UpdateMessage) {}
        fn send_state(&self, _state: &crate::SyncMessage) {}
        fn close(self) {}
    }

    // Create a simple test config using Default if available, otherwise minimal valid config
    fn create_test_multiple_choice_config() -> SlideConfig {
        // Use a valid slide config that can be created through public APIs
        SlideConfig::MultipleChoice(
            serde_json::from_str(
                r#"{
                "title": "Test Question",
                "media": null,
                "introduce_question": 2,
                "time_limit": 30,
                "points_awarded": 1000,
                "answers": [
                    {"correct": true, "content": {"Text": "Answer A"}},
                    {"correct": false, "content": {"Text": "Answer B"}}
                ]
            }"#,
            )
            .unwrap(),
        )
    }

    fn create_test_type_answer_config() -> SlideConfig {
        SlideConfig::TypeAnswer(
            serde_json::from_str(
                r#"{
                "title": "Test Type Answer",
                "media": null,
                "introduce_question": 2,
                "time_limit": 30,
                "points_awarded": 1000,
                "answers": ["test", "TEST"],
                "case_sensitive": false
            }"#,
            )
            .unwrap(),
        )
    }

    fn create_test_order_config() -> SlideConfig {
        SlideConfig::Order(
            serde_json::from_str(
                r#"{
                "title": "Test Order",
                "media": null,
                "introduce_question": 2,
                "time_limit": 30,
                "points_awarded": 1000,
                "answers": ["First", "Second", "Third"],
                "axis_labels": {"from": "Start", "to": "End"}
            }"#,
            )
            .unwrap(),
        )
    }

    fn create_mock_watchers() -> Watchers {
        Watchers::new(1000)
    }

    fn create_mock_tunnel_finder() -> impl Fn(Id) -> Option<MockTunnel> {
        |_id: Id| Some(MockTunnel)
    }

    fn create_mock_leaderboard() -> Leaderboard {
        Leaderboard::default()
    }

    #[test]
    fn test_slide_config_to_state_multiple_choice() {
        let mc_config = create_test_multiple_choice_config();
        let state = mc_config.to_state();

        match state {
            SlideState::MultipleChoice(_) => {
                // Successfully created MultipleChoice state
            }
            _ => panic!("Expected MultipleChoice state"),
        }
    }

    #[test]
    fn test_slide_config_to_state_type_answer() {
        let ta_config = create_test_type_answer_config();
        let state = ta_config.to_state();

        match state {
            SlideState::TypeAnswer(_) => {
                // Successfully created TypeAnswer state
            }
            _ => panic!("Expected TypeAnswer state"),
        }
    }

    #[test]
    fn test_slide_config_to_state_order() {
        let order_config = create_test_order_config();
        let state = order_config.to_state();

        match state {
            SlideState::Order(_) => {
                // Successfully created Order state
            }
            _ => panic!("Expected Order state"),
        }
    }

    #[test]
    fn test_slide_state_play_multiple_choice() {
        let mc_config = create_test_multiple_choice_config();
        let mut state = mc_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut schedule_called = false;
        let schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {
            schedule_called = true;
        };

        state.play(None, &watchers, schedule_message, Tick::default(), tunnel_finder, 0, 1);

        // Verify play was called successfully (schedule message was triggered)
        assert!(schedule_called);
    }

    #[test]
    fn test_slide_state_play_type_answer() {
        let ta_config = create_test_type_answer_config();
        let mut state = ta_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut schedule_called = false;
        let schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {
            schedule_called = true;
        };

        state.play(None, &watchers, schedule_message, Tick::default(), tunnel_finder, 0, 1);

        // Verify play was called successfully (schedule message was triggered)
        assert!(schedule_called);
    }

    #[test]
    fn test_slide_state_play_order() {
        let order_config = create_test_order_config();
        let mut state = order_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut schedule_called = false;
        let schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {
            schedule_called = true;
        };

        state.play(None, &watchers, schedule_message, Tick::default(), tunnel_finder, 0, 1);

        // Verify play was called successfully (schedule message was triggered)
        assert!(schedule_called);
    }

    #[test]
    fn test_slide_state_receive_message_multiple_choice() {
        let mc_config = create_test_multiple_choice_config();
        let mut state = mc_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut leaderboard = create_mock_leaderboard();
        let schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {};
        let message = IncomingMessage::Host(IncomingHostMessage::Next(crate::game::HostScreen::Lobby));

        let _result = state.receive_message(
            &mut leaderboard,
            &watchers,
            None,
            schedule_message,
            Tick::default(),
            Id::new(),
            tunnel_finder,
            message,
            0,
            1,
        );

        // Verify the message was processed (result may be true or false depending on message processing)
        // The important thing is that the method was called without panicking
    }

    #[test]
    fn test_slide_state_receive_message_type_answer() {
        let ta_config = create_test_type_answer_config();
        let mut state = ta_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut leaderboard = create_mock_leaderboard();
        let schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {};
        let message = IncomingMessage::Host(IncomingHostMessage::Next(crate::game::HostScreen::Lobby));

        let _result = state.receive_message(
            &mut leaderboard,
            &watchers,
            None,
            schedule_message,
            Tick::default(),
            Id::new(),
            tunnel_finder,
            message,
            0,
            1,
        );

        // Verify the message was processed (result may be true or false depending on message processing)
        // The important thing is that the method was called without panicking
    }

    #[test]
    fn test_slide_state_receive_message_order() {
        let order_config = create_test_order_config();
        let mut state = order_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut leaderboard = create_mock_leaderboard();
        let schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {};
        let message = IncomingMessage::Host(IncomingHostMessage::Next(crate::game::HostScreen::Lobby));

        let _result = state.receive_message(
            &mut leaderboard,
            &watchers,
            None,
            schedule_message,
            Tick::default(),
            Id::new(),
            tunnel_finder,
            message,
            0,
            1,
        );

        // Verify the message was processed (result may be true or false depending on message processing)
        // The important thing is that the method was called without panicking
    }

    #[test]
    fn test_slide_state_state_message_multiple_choice() {
        let mc_config = create_test_multiple_choice_config();
        let state = mc_config.to_state();
        let tunnel_finder = create_mock_tunnel_finder();

        let message = state.state_message(Id::new(), ValueKind::Player, None, tunnel_finder, 0, 1);

        match message {
            SyncMessage::MultipleChoice(_) => {}
            _ => panic!("Expected MultipleChoice sync message"),
        }
    }

    #[test]
    fn test_slide_state_state_message_type_answer() {
        let ta_config = create_test_type_answer_config();
        let state = ta_config.to_state();
        let tunnel_finder = create_mock_tunnel_finder();

        let message = state.state_message(Id::new(), ValueKind::Player, None, tunnel_finder, 0, 1);

        match message {
            SyncMessage::TypeAnswer(_) => {}
            _ => panic!("Expected TypeAnswer sync message"),
        }
    }

    #[test]
    fn test_slide_state_state_message_order() {
        let order_config = create_test_order_config();
        let state = order_config.to_state();
        let tunnel_finder = create_mock_tunnel_finder();

        let message = state.state_message(Id::new(), ValueKind::Player, None, tunnel_finder, 0, 1);

        match message {
            SyncMessage::Order(_) => {}
            _ => panic!("Expected Order sync message"),
        }
    }

    #[test]
    fn test_fuiz_len_and_is_empty() {
        let empty_fuiz = Fuiz {
            title: "Empty".to_string(),
            slides: vec![],
        };
        assert_eq!(empty_fuiz.len(), 0);
        assert!(empty_fuiz.is_empty());

        let fuiz_with_slides = Fuiz {
            title: "With Slides".to_string(),
            slides: vec![create_test_multiple_choice_config(), create_test_type_answer_config()],
        };
        assert_eq!(fuiz_with_slides.len(), 2);
        assert!(!fuiz_with_slides.is_empty());
    }

    #[cfg(feature = "serializable")]
    #[test]
    fn test_current_slide_serialization() {
        let mc_config = create_test_multiple_choice_config();
        let slide_state = mc_config.to_state();
        let current_slide = CurrentSlide {
            index: 0,
            state: slide_state,
        };

        // Test serialization doesn't panic
        let _serialized = serde_json::to_string(&current_slide).unwrap();
    }

    #[test]
    fn test_text_or_media_validation() {
        // Valid text
        let valid_text = TextOrMedia::Text("Valid text".to_string());
        assert!(valid_text.validate().is_ok());

        // Text too long
        let long_text = TextOrMedia::Text("x".repeat(crate::settings::AnswerTextSettings::default().max_length + 1));
        assert!(long_text.validate().is_err());
    }

    #[test]
    fn test_slide_state_receive_alarm_type_answer() {
        let ta_config = create_test_type_answer_config();
        let mut state = ta_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {};

        let alarm_message = AlarmMessage::TypeAnswer(type_answer::AlarmMessage {
            index: 0,
            to: type_answer::Phase::Question,
        });

        let _result = state.receive_alarm(
            &mut create_mock_leaderboard(),
            &watchers,
            None,
            &mut schedule_message,
            Tick::default(),
            tunnel_finder,
            &alarm_message,
            0,
            1,
        );

        // Test completed successfully - receive_alarm was called on TypeAnswer variant
    }

    #[test]
    fn test_slide_state_receive_alarm_order() {
        let order_config = create_test_order_config();
        let mut state = order_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {};

        let alarm_message = AlarmMessage::Order(order::AlarmMessage {
            index: 0,
            to: order::Phase::Question,
        });

        let _result = state.receive_alarm(
            &mut create_mock_leaderboard(),
            &watchers,
            None,
            &mut schedule_message,
            Tick::default(),
            tunnel_finder,
            &alarm_message,
            0,
            1,
        );

        // Test completed successfully - receive_alarm was called on Order variant
    }

    #[test]
    fn test_slide_state_receive_alarm_multiple_choice() {
        let mc_config = create_test_multiple_choice_config();
        let mut state = mc_config.to_state();
        let watchers = create_mock_watchers();
        let tunnel_finder = create_mock_tunnel_finder();
        let mut schedule_message = |_msg: AlarmMessage, _duration: std::time::Duration| {};

        let alarm_message = AlarmMessage::MultipleChoice(multiple_choice::AlarmMessage {
            index: 0,
            to: multiple_choice::Phase::Question,
        });

        let _result = state.receive_alarm(
            &mut create_mock_leaderboard(),
            &watchers,
            None,
            &mut schedule_message,
            Tick::default(),
            tunnel_finder,
            &alarm_message,
            0,
            1,
        );

        // Test completed successfully - receive_alarm was called on MultipleChoice variant
    }

    // ---------- the collect-opinions and present-info slide types ----------

    /// Each of these doubles as a check that the wire format the website sends
    /// still deserializes into the slide config it is meant to.
    fn parse_slide(json: &str) -> SlideConfig {
        serde_json::from_str(json).expect("slide config should deserialize")
    }

    /// A config paired with the predicate that recognises what it should
    /// become: a state variant, a sync message kind, and so on.
    type SlideCase<T> = (SlideConfig, fn(&T) -> bool);

    /// Same idea for sync messages, but the predicate has to be valid for any
    /// borrow of the state it was built from, hence the explicit `for<'a>`.
    type SyncCase = (SlideConfig, for<'a> fn(&SyncMessage<'a>) -> bool);

    fn create_test_slider_config() -> SlideConfig {
        parse_slide(
            r#"{"Slider": {
                "title": "How tall is the Eiffel Tower?",
                "media": null,
                "introduce_question": 2000,
                "time_limit": 30000,
                "points_awarded": 1000,
                "range": {"min": 0.0, "max": 500.0, "step": 5.0},
                "correct": 330.0,
                "tolerance": 10.0,
                "unit": "m"
            }}"#,
        )
    }

    fn create_test_scale_config() -> SlideConfig {
        parse_slide(
            r#"{"Scale": {
                "title": "How was the lesson?",
                "media": null,
                "points_awarded": 0,
                "min": 1,
                "max": 5,
                "style": "Agreement",
                "labels": {"low": "Awful", "mid": null, "high": "Great"}
            }}"#,
        )
    }

    fn create_test_poll_config() -> SlideConfig {
        parse_slide(
            r#"{"Poll": {
                "title": "Which should we do next?",
                "media": null,
                "points_awarded": 0,
                "answers": [{"Text": "Revision"}, {"Text": "New topic"}]
            }}"#,
        )
    }

    fn create_test_pin_config() -> SlideConfig {
        parse_slide(
            r#"{"Pin": {
                "title": "Where is Rome?",
                "media": null,
                "points_awarded": 1000,
                "correct_area": {
                    "Ellipse": {
                        "center": {"x": 0.5, "y": 0.4},
                        "radius_x": 0.08,
                        "radius_y": 0.12
                    }
                }
            }}"#,
        )
    }

    fn create_test_free_text_config() -> SlideConfig {
        parse_slide(
            r#"{"FreeText": {
                "title": "One word for today?",
                "media": null,
                "points_awarded": 0,
                "mode": "WordCloud",
                "max_entries": 3,
                "max_entry_length": 40
            }}"#,
        )
    }

    fn create_test_brainstorm_config() -> SlideConfig {
        parse_slide(
            r#"{"Brainstorm": {
                "title": "How do we cut waste?",
                "media": null,
                "points_awarded": 0,
                "idea_time_limit": 60000,
                "vote_time_limit": 30000,
                "max_ideas_per_player": 3,
                "max_votes_per_player": 2,
                "max_idea_length": 100
            }}"#,
        )
    }

    fn create_test_info_slide_config() -> SlideConfig {
        parse_slide(
            r#"{"InfoSlide": {
                "title": "A word on photosynthesis",
                "body": "Plants turn light into sugar.",
                "media": null,
                "duration": 10000
            }}"#,
        )
    }

    fn every_new_slide_config() -> Vec<SlideConfig> {
        vec![
            create_test_slider_config(),
            create_test_scale_config(),
            create_test_poll_config(),
            create_test_pin_config(),
            create_test_free_text_config(),
            create_test_brainstorm_config(),
            create_test_info_slide_config(),
        ]
    }

    #[test]
    fn every_new_slide_config_validates() {
        let settings = crate::settings::Settings::default();
        for config in every_new_slide_config() {
            assert!(
                config.validate_with(&settings).is_ok(),
                "config should pass validation: {config:?}"
            );
        }
    }

    #[test]
    fn every_new_slide_config_maps_to_its_own_state() {
        let pairs: Vec<SlideCase<SlideState>> = vec![
            (create_test_slider_config(), |s| matches!(s, SlideState::Slider(_))),
            (create_test_scale_config(), |s| matches!(s, SlideState::Scale(_))),
            (create_test_poll_config(), |s| matches!(s, SlideState::Poll(_))),
            (create_test_pin_config(), |s| matches!(s, SlideState::Pin(_))),
            (create_test_free_text_config(), |s| matches!(s, SlideState::FreeText(_))),
            (create_test_brainstorm_config(), |s| {
                matches!(s, SlideState::Brainstorm(_))
            }),
            (create_test_info_slide_config(), |s| {
                matches!(s, SlideState::InfoSlide(_))
            }),
        ];

        for (config, is_expected_variant) in pairs {
            let state = config.to_state();
            assert!(is_expected_variant(&state), "unexpected state for {config:?}");
        }
    }

    #[test]
    fn every_new_slide_syncs_under_its_own_message_kind() {
        let tunnel_finder = create_mock_tunnel_finder();
        let checks: Vec<SyncCase> = vec![
            (create_test_slider_config(), |m| matches!(m, SyncMessage::Slider(_))),
            (create_test_scale_config(), |m| matches!(m, SyncMessage::Scale(_))),
            (create_test_poll_config(), |m| matches!(m, SyncMessage::Poll(_))),
            (create_test_pin_config(), |m| matches!(m, SyncMessage::Pin(_))),
            (create_test_free_text_config(), |m| {
                matches!(m, SyncMessage::FreeText(_))
            }),
            (create_test_brainstorm_config(), |m| {
                matches!(m, SyncMessage::Brainstorm(_))
            }),
            (create_test_info_slide_config(), |m| {
                matches!(m, SyncMessage::InfoSlide(_))
            }),
        ];

        for (config, is_expected_kind) in checks {
            let state = config.to_state();
            let matched = {
                let message = state.state_message(Id::new(), ValueKind::Player, None, &tunnel_finder, 0, 1);
                is_expected_kind(&message)
            };
            assert!(matched, "unexpected sync message for {config:?}");
        }
    }

    #[test]
    fn every_new_slide_starts_without_panicking() {
        for config in every_new_slide_config() {
            let mut state = config.to_state();
            let watchers = create_mock_watchers();
            let mut scheduled = 0;
            state.play(
                None,
                &watchers,
                |_msg: AlarmMessage, _duration: std::time::Duration| scheduled += 1,
                Tick::default(),
                create_mock_tunnel_finder(),
                0,
                1,
            );
        }
    }

    #[test]
    fn only_scoring_slides_earn_a_leaderboard_screen() {
        assert!(create_test_slider_config().to_state().awards_points());
        assert!(create_test_pin_config().to_state().awards_points());
        assert!(create_test_multiple_choice_config().to_state().awards_points());

        for opinion in [
            create_test_scale_config(),
            create_test_poll_config(),
            create_test_free_text_config(),
            create_test_brainstorm_config(),
            create_test_info_slide_config(),
        ] {
            assert!(
                !opinion.to_state().awards_points(),
                "opinion and info slides skip the standings screen: {opinion:?}"
            );
        }
    }

    #[test]
    fn a_drop_pin_scores_nothing_while_a_pin_answer_does() {
        let drop_pin = parse_slide(
            r#"{"Pin": {
                "title": "Where did you grow up?",
                "media": null,
                "points_awarded": 0,
                "correct_area": null
            }}"#,
        );
        assert!(!drop_pin.to_state().awards_points());
        assert!(create_test_pin_config().to_state().awards_points());
    }
}
