//! Fuiz configuration and question management
//!
//! This module defines the core configuration structures for Fuiz games,
//! including the main `Fuiz` struct, slide configurations, and the runtime
//! state management for different question types. It provides the central
//! coordination layer that manages question flow and state transitions.

use garde::Validate;
use serde::{Deserialize, Serialize};

use super::{super::game::IncomingMessage, media::Media, multiple_choice, order, type_answer};
use crate::fuiz::common::QuestionReceiveMessage;
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

/// Represents content that can be either text or media
///
/// This enum allows questions and answers to include either plain text
/// or rich media content like images, providing flexibility in question design.
#[derive(Debug, Serialize, Deserialize, Clone, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub enum TextOrMedia {
    /// Media content (images, etc.)
    Media(#[garde(skip)] Media),
    /// Plain text content with length validation
    Text(#[garde(length(max = ctx.answer_text.max_length))] String),
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
#[derive(Debug, Serialize, Deserialize, Clone)]
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
        }
    }
}

/// Runtime state for a slide during active gameplay
///
/// This enum represents the active state of a slide while it's being
/// presented to players. It maintains timing information, player responses,
/// and current phase information for each question type.
#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum SlideState {
    /// Runtime state for a multiple choice question
    MultipleChoice(multiple_choice::State),
    /// Runtime state for a type answer question
    TypeAnswer(type_answer::State),
    /// Runtime state for an order question
    Order(order::State),
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
    },
    /// Stay on the current slide, potentially changing its state
    Stay,
}

impl SlideState {
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
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        match self {
            Self::MultipleChoice(s) => {
                s.play(team_manager, watchers, schedule_message, tunnel_finder, index, count);
            }
            Self::TypeAnswer(s) => {
                s.play(watchers, schedule_message, tunnel_finder, index, count);
            }
            Self::Order(s) => {
                s.play(watchers, schedule_message, tunnel_finder, index, count);
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
        watcher_id: Id,
        tunnel_finder: F,
        message: IncomingMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        match self {
            Self::MultipleChoice(s) => s.receive_message(
                watcher_id,
                message,
                leaderboard,
                watchers,
                team_manager,
                schedule_message,
                tunnel_finder,
                index,
                count,
            ),
            Self::TypeAnswer(s) => s.receive_message(
                watcher_id,
                message,
                leaderboard,
                watchers,
                team_manager,
                schedule_message,
                tunnel_finder,
                index,
                count,
            ),
            Self::Order(s) => s.receive_message(
                watcher_id,
                message,
                leaderboard,
                watchers,
                team_manager,
                schedule_message,
                tunnel_finder,
                index,
                count,
            ),
        }
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
    ) -> SyncMessage {
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
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tunnel_finder: F,
        message: &AlarmMessage,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        match self {
            Self::MultipleChoice(s) => {
                s.receive_alarm(watchers, team_manager, schedule_message, tunnel_finder, message, index)
            }
            Self::TypeAnswer(s) => s.receive_alarm(watchers, schedule_message, tunnel_finder, message, index, count),
            Self::Order(s) => s.receive_alarm(watchers, schedule_message, tunnel_finder, message, index, count),
        }
    }

    /// Notify the active slide that a watcher has gone offline so it can
    /// keep its live-answered counter in sync.
    pub(crate) fn mark_watcher_left(&mut self, id: crate::watcher::Id) {
        use crate::fuiz::common::AnswerHandler;
        match self {
            Self::MultipleChoice(s) => s.mark_watcher_left(id),
            Self::TypeAnswer(s) => s.mark_watcher_left(id),
            Self::Order(s) => s.mark_watcher_left(id),
        }
    }

    /// Notify the active slide that a watcher has reconnected so it can
    /// keep its live-answered counter in sync.
    pub(crate) fn mark_watcher_returned(&mut self, id: crate::watcher::Id) {
        use crate::fuiz::common::AnswerHandler;
        match self {
            Self::MultipleChoice(s) => s.mark_watcher_returned(id),
            Self::TypeAnswer(s) => s.mark_watcher_returned(id),
            Self::Order(s) => s.mark_watcher_returned(id),
        }
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

        state.play(None, &watchers, schedule_message, tunnel_finder, 0, 1);

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

        state.play(None, &watchers, schedule_message, tunnel_finder, 0, 1);

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

        state.play(None, &watchers, schedule_message, tunnel_finder, 0, 1);

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
        let message = IncomingMessage::Host(IncomingHostMessage::Next);

        let _result = state.receive_message(
            &mut leaderboard,
            &watchers,
            None,
            schedule_message,
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
        let message = IncomingMessage::Host(IncomingHostMessage::Next);

        let _result = state.receive_message(
            &mut leaderboard,
            &watchers,
            None,
            schedule_message,
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
        let message = IncomingMessage::Host(IncomingHostMessage::Next);

        let _result = state.receive_message(
            &mut leaderboard,
            &watchers,
            None,
            schedule_message,
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

        let alarm_message = AlarmMessage::TypeAnswer(type_answer::AlarmMessage::ProceedFromSlideIntoSlide {
            index: 0,
            to: type_answer::SlideState::Question,
        });

        let _result = state.receive_alarm(
            &watchers,
            None,
            &mut schedule_message,
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

        let alarm_message = AlarmMessage::Order(order::AlarmMessage::ProceedFromSlideIntoSlide {
            index: 0,
            to: order::SlideState::Question,
        });

        let _result = state.receive_alarm(
            &watchers,
            None,
            &mut schedule_message,
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

        let alarm_message = AlarmMessage::MultipleChoice(multiple_choice::AlarmMessage::ProceedFromSlideIntoSlide {
            index: 0,
            to: multiple_choice::SlideState::Question,
        });

        let _result = state.receive_alarm(
            &watchers,
            None,
            &mut schedule_message,
            tunnel_finder,
            &alarm_message,
            0,
            1,
        );

        // Test completed successfully - receive_alarm was called on MultipleChoice variant
    }
}
