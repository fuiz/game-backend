//! # Fuiz Game Library
//!
//! This library provides the core game logic for the Fuiz quiz game system.
//! It handles game sessions, player management, different question types,
//! leaderboards, and real-time synchronization between players and hosts.

#![cfg_attr(all(coverage_nightly, test), feature(coverage_attribute))]
use derive_where::derive_where;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

pub mod settings;

pub mod fuiz;
pub mod game;
pub mod game_id;
pub mod leaderboard;
mod names;
pub mod session;
pub mod teams;
pub mod time;
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
}

/// A truncated vector that maintains the exact count while limiting displayed items
///
/// This structure is useful for displaying a limited number of items while
/// still showing the total count. For example, showing "10 players" but only
/// displaying the first 5 names.
#[derive(Debug, Clone, Serialize)]
#[derive_where(Default)]
pub struct TruncatedVec<T> {
    /// The exact total count of items
    exact_count: usize,
    /// The truncated list of items (up to the limit)
    items: Vec<T>,
}

impl<T: Clone> TruncatedVec<T> {
    /// Creates a new truncated vector from an iterator
    ///
    /// # Arguments
    ///
    /// * `list` - An iterator over items to include
    /// * `limit` - Maximum number of items to include in the truncated vector
    /// * `exact_count` - The exact total count of items (may be larger than limit)
    ///
    /// # Returns
    ///
    /// A new `TruncatedVec` containing up to `limit` items from the iterator
    pub fn new<I: Iterator<Item = T>>(list: I, limit: usize, exact_count: usize) -> Self {
        let items = list.take(limit).collect_vec();
        Self { exact_count, items }
    }

    /// Maps a function over the items in the truncated vector
    ///
    /// # Arguments
    ///
    /// * `f` - Function to apply to each item
    ///
    /// # Returns
    ///
    /// A new `TruncatedVec` with the function applied to each item
    pub fn map<F, U>(self, f: F) -> TruncatedVec<U>
    where
        F: Fn(T) -> U,
    {
        TruncatedVec {
            exact_count: self.exact_count,
            items: self.items.into_iter().map(f).collect_vec(),
        }
    }

    /// Returns the exact count of items
    pub fn exact_count(&self) -> usize {
        self.exact_count
    }

    /// Returns the truncated items
    pub fn items(&self) -> &[T] {
        &self.items
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn test_truncated_vec_new() {
        let data = vec![1, 2, 3, 4, 5];
        let truncated = TruncatedVec::new(data.into_iter(), 3, 5);

        assert_eq!(truncated.exact_count(), 5);
        assert_eq!(truncated.items(), &[1, 2, 3]);
    }

    #[test]
    fn test_truncated_vec_new_limit_larger_than_items() {
        let data = vec![1, 2, 3];
        let truncated = TruncatedVec::new(data.into_iter(), 5, 3);

        assert_eq!(truncated.exact_count(), 3);
        assert_eq!(truncated.items(), &[1, 2, 3]);
    }

    #[test]
    fn test_truncated_vec_new_empty() {
        let data: Vec<i32> = vec![];
        let truncated = TruncatedVec::new(data.into_iter(), 5, 0);

        assert_eq!(truncated.exact_count(), 0);
        let empty: &[i32] = &[];
        assert_eq!(truncated.items(), empty);
    }

    #[test]
    fn test_truncated_vec_map() {
        let data = vec![1, 2, 3];
        let truncated = TruncatedVec::new(data.into_iter(), 3, 5);
        let mapped = truncated.map(|x| x * 2);

        assert_eq!(mapped.exact_count(), 5);
        assert_eq!(mapped.items(), &[2, 4, 6]);
    }

    #[test]
    fn test_truncated_vec_map_string() {
        let data = vec![1, 2, 3];
        let truncated = TruncatedVec::new(data.into_iter(), 2, 3);
        let mapped = truncated.map(|x| format!("item_{x}"));

        assert_eq!(mapped.exact_count(), 3);
        assert_eq!(mapped.items(), &["item_1", "item_2"]);
    }

    #[test]
    fn test_sync_message_to_message() {
        let players = TruncatedVec::new(["Player1", "Player2"].into_iter(), 10, 2);
        let sync_msg = SyncMessage::Game(crate::game::SyncMessage::WaitingScreen(players));
        let json_str = serde_json::to_string(&sync_msg).expect("default serializer cannot fail");

        assert!(json_str.contains("Game"));
        assert!(json_str.contains("WaitingScreen"));
    }

    #[test]
    fn test_update_message_to_message() {
        let players = TruncatedVec::new(["Player1"].into_iter(), 10, 1);
        let update_msg = UpdateMessage::Game(crate::game::UpdateMessage::WaitingScreen(players));
        let json_str = serde_json::to_string(&update_msg).expect("default serializer cannot fail");

        assert!(json_str.contains("Game"));
        assert!(json_str.contains("WaitingScreen"));
        assert!(json_str.contains("Player1"));
    }
}
