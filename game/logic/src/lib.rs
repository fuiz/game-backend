//! # Fuiz Game Library
//!
//! This library provides the core game logic for the Fuiz quiz game system.
//! It handles game sessions, player management, different question types,
//! leaderboards, and real-time synchronization between players and hosts.

#![cfg_attr(all(coverage_nightly, test), feature(coverage_attribute))]
use serde::{Deserialize, Serialize};

pub mod settings;

pub mod fuiz;
pub mod game;
pub mod game_id;
pub mod leaderboard;
mod names;
pub mod session;
pub mod teams;
pub mod tick;
pub mod time;
// Replaying a log means reconstructing persisted state, which is exactly what
// `serializable` gates. Without it a `Game` cannot round-trip at all, so the
// log would have nothing to rebuild from.
#[cfg(feature = "serializable")]
pub mod wal;
pub mod watcher;

/// Messages sent to synchronize state between players and hosts
///
/// This enum represents all possible synchronization messages that can be
/// sent to keep game state consistent across all connected clients.
#[derive(Debug, Serialize, Clone, derive_more::From)]
pub enum SyncMessage<'a> {
    /// General game synchronization messages
    Game(game::SyncMessage<'a>),
    /// Multiple choice question synchronization
    MultipleChoice(fuiz::multiple_choice::SyncMessage<'a>),
    /// Type answer question synchronization
    TypeAnswer(fuiz::type_answer::SyncMessage<'a>),
    /// Order question synchronization
    Order(fuiz::order::SyncMessage<'a>),
    /// Slider question synchronization
    Slider(fuiz::slider::SyncMessage<'a>),
    /// Scale (agreement / NPS) question synchronization
    Scale(fuiz::scale::SyncMessage<'a>),
    /// Poll synchronization
    Poll(fuiz::poll::SyncMessage<'a>),
    /// Pin (pin answer / drop pin) question synchronization
    Pin(fuiz::pin::SyncMessage<'a>),
    /// Free-text (word cloud / open ended) question synchronization
    FreeText(fuiz::free_text::SyncMessage<'a>),
    /// Brainstorm synchronization
    Brainstorm(fuiz::brainstorm::SyncMessage<'a>),
    /// Info slide synchronization
    InfoSlide(fuiz::info_slide::SyncMessage<'a>),
}

/// Messages sent to update specific aspects of the game state
///
/// Update messages are used to notify clients about changes that affect
/// their local view of the game, such as score updates or new questions.
#[derive(Debug, Serialize, Clone, derive_more::From)]
pub enum UpdateMessage<'a> {
    /// General game update messages
    Game(game::UpdateMessage<'a>),
    /// Multiple choice question updates
    MultipleChoice(fuiz::multiple_choice::UpdateMessage<'a>),
    /// Type answer question updates
    TypeAnswer(fuiz::type_answer::UpdateMessage<'a>),
    /// Order question updates
    Order(fuiz::order::UpdateMessage<'a>),
    /// Slider question updates
    Slider(fuiz::slider::UpdateMessage<'a>),
    /// Scale (agreement / NPS) question updates
    Scale(fuiz::scale::UpdateMessage<'a>),
    /// Poll updates
    Poll(fuiz::poll::UpdateMessage<'a>),
    /// Pin (pin answer / drop pin) question updates
    Pin(fuiz::pin::UpdateMessage<'a>),
    /// Free-text (word cloud / open ended) question updates
    FreeText(fuiz::free_text::UpdateMessage<'a>),
    /// Brainstorm updates
    Brainstorm(fuiz::brainstorm::UpdateMessage<'a>),
    /// Info slide updates
    InfoSlide(fuiz::info_slide::UpdateMessage<'a>),
}

/// Alarm messages for timed events in different question types
///
/// These messages are used to handle time-based events like question
/// timeouts or countdown warnings.
#[derive(Debug, Clone, derive_more::From, Serialize, Deserialize)]
pub enum AlarmMessage {
    /// Multiple choice question alarms
    MultipleChoice(fuiz::multiple_choice::AlarmMessage),
    /// Type answer question alarms
    TypeAnswer(fuiz::type_answer::AlarmMessage),
    /// Order question alarms
    Order(fuiz::order::AlarmMessage),
    /// Slider question alarms
    Slider(fuiz::slider::AlarmMessage),
    /// Scale (agreement / NPS) question alarms
    Scale(fuiz::scale::AlarmMessage),
    /// Poll alarms
    Poll(fuiz::poll::AlarmMessage),
    /// Pin (pin answer / drop pin) question alarms
    Pin(fuiz::pin::AlarmMessage),
    /// Free-text (word cloud / open ended) question alarms
    FreeText(fuiz::free_text::AlarmMessage),
    /// Brainstorm alarms
    Brainstorm(fuiz::brainstorm::AlarmMessage),
    /// Info slide alarms
    InfoSlide(fuiz::info_slide::AlarmMessage),
    /// Advances a host-paced screen once its auto-advance delay runs out
    AutoAdvance(game::AutoAdvanceAlarm),
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_sync_message_to_message() {
        let players = vec!["Player1", "Player2"];
        let sync_msg = SyncMessage::Game(crate::game::SyncMessage::WaitingScreen(players));
        let json_str = serde_json::to_string(&sync_msg).expect("default serializer cannot fail");

        assert!(json_str.contains("Game"));
        assert!(json_str.contains("WaitingScreen"));
        assert!(json_str.contains("Player1"));
    }

    #[test]
    fn test_update_message_to_message() {
        let update_msg = UpdateMessage::Game(crate::game::UpdateMessage::PlayerJoined("Player1"));
        let json_str = serde_json::to_string(&update_msg).expect("default serializer cannot fail");

        assert!(json_str.contains("Game"));
        assert!(json_str.contains("PlayerJoined"));
        assert!(json_str.contains("Player1"));
    }
}
