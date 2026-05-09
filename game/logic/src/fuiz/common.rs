//! Shared traits and common functionality for slide implementations
//!
//! This module contains traits and helper functions that are common across
//! different question types (multiple_choice, type_answer, order), reducing
//! code duplication and providing consistent behavior.

use std::{
    collections::{HashMap, hash_map::Keys},
    iter::Copied,
    time::Duration,
};

use itertools::Itertools;
use serde::{Deserialize, Serialize};
use web_time::SystemTime;

use crate::{
    fuiz::config::{ScheduleMessageFn, SlideAction},
    leaderboard::Leaderboard,
    session::TunnelFinder,
    teams::TeamManager,
    watcher::{Id, ValueKind, Watchers},
};

/// Common slide states shared by all question types
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SlideState {
    /// Initial state before the slide has started
    #[default]
    Unstarted,
    /// Displaying the question without answers
    Question,
    /// Accepting answers from players
    Answers,
    /// Displaying results with correct answers and statistics
    AnswersResults,
}

/// Trait for basic slide state management functionality
pub trait SlideStateManager {
    /// Get the current slide state
    fn state(&self) -> SlideState;

    /// Attempt to change state from one to another
    /// Returns true if successful, false if current state doesn't match expected
    fn change_state(&mut self, before: SlideState, after: SlideState) -> bool;
}

/// Trait for slide timer management
pub trait SlideTimer {
    /// Get the answer start time, or None if not started
    fn answer_start(&self) -> Option<SystemTime>;

    /// Set the answer start time, or None if not started
    fn set_answer_start(&mut self, time: Option<SystemTime>);

    /// Start the timer by setting the current time
    fn start_timer(&mut self) {
        self.set_answer_start(Some(SystemTime::now()));
    }

    /// Get the timer start time, or current time if not set
    fn timer(&self) -> SystemTime {
        self.answer_start().unwrap_or(SystemTime::now())
    }

    /// Get the elapsed time since the timer started
    fn elapsed(&self) -> Duration {
        self.timer().elapsed().unwrap_or_default()
    }
}

/// Calculate score based on timing - shared function used by all slide types
///
/// When `full_duration` is `None` (host-paced mode), full points are awarded
/// regardless of how long the player took to answer.
pub fn calculate_slide_score(
    full_duration: Option<Duration>,
    taken_duration: Duration,
    full_points_awarded: u64,
) -> u64 {
    match full_duration {
        Some(full) => {
            (full_points_awarded as f64 * (1. - (taken_duration.as_secs_f64() / full.as_secs_f64() / 2.))) as u64
        }
        None => full_points_awarded,
    }
}

/// Trait for slides that handle answers and scoring
pub trait AnswerHandler<AnswerType> {
    /// Get user answers with timestamps
    fn user_answers(&self) -> &HashMap<Id, (AnswerType, SystemTime)>;

    /// Get mutable access to user answers with timestamps
    fn user_answers_mut(&mut self) -> &mut HashMap<Id, (AnswerType, SystemTime)>;

    /// Number of distinct *live* players who have answered.
    ///
    /// Maintained incrementally by [`Self::record_answer`],
    /// [`Self::mark_watcher_left`], and [`Self::mark_watcher_returned`] so
    /// that "all answered" / "answered count" checks are O(1) instead of
    /// scanning the watcher set per answer.
    fn live_answered_count(&self) -> usize;

    /// Mutable handle to the live-answered counter. Implementors expose the
    /// same field that [`Self::live_answered_count`] reads.
    fn live_answered_count_mut(&mut self) -> &mut usize;

    /// Get the IDs of players who have answered
    fn ids_of_who_answered(&self) -> Copied<Keys<'_, Id, (AnswerType, SystemTime)>> {
        self.user_answers().keys().copied()
    }

    /// Records a player's answer with the current timestamp.
    ///
    /// Updates the live-answered counter when this is the first answer from
    /// `id` (so a player overwriting their own answer doesn't double-count).
    fn record_answer(&mut self, id: Id, answer: AnswerType) {
        let transformed_answer = self.transform_answer(answer);
        let was_new = self
            .user_answers_mut()
            .insert(id, (transformed_answer, SystemTime::now()))
            .is_none();
        if was_new {
            *self.live_answered_count_mut() += 1;
        }
    }

    /// Notify the slide that a watcher has gone offline.
    ///
    /// If the watcher had already answered, decrement the live-answered count
    /// (their answer stays in `user_answers` so it still scores at slide end).
    fn mark_watcher_left(&mut self, id: Id) {
        if self.user_answers().contains_key(&id) {
            let c = self.live_answered_count_mut();
            *c = c.saturating_sub(1);
        }
    }

    /// Notify the slide that a watcher has reconnected.
    ///
    /// If they had previously answered (and were de-counted by
    /// [`Self::mark_watcher_left`]), put them back in the live-answered tally.
    fn mark_watcher_returned(&mut self, id: Id) {
        if self.user_answers().contains_key(&id) {
            *self.live_answered_count_mut() += 1;
        }
    }

    /// Transforms the player's answer before recording it
    fn transform_answer(&self, answer: AnswerType) -> AnswerType {
        answer
    }

    /// Get the counts of each unique answer
    fn answer_counts(&self) -> HashMap<AnswerType, usize>
    where
        AnswerType: Clone + Eq + std::hash::Hash,
    {
        self.user_answers()
            .iter()
            .map(|(_, (answer, _))| answer.to_owned())
            .counts()
    }

    /// Get the count of correct answers
    fn correct_count(&self) -> usize {
        self.user_answers()
            .iter()
            .filter(|(_, (answer, _))| self.is_correct_answer(answer))
            .count()
    }

    /// Check if an answer is correct
    fn is_correct_answer(&self, answer: &AnswerType) -> bool;

    /// Returns a score multiplier for the given answer (0.0 to 1.0).
    /// Default implementation returns 1.0 for correct, 0.0 for incorrect.
    fn score_multiplier(&self, answer: &AnswerType) -> f64 {
        if self.is_correct_answer(answer) { 1.0 } else { 0.0 }
    }

    /// Get the maximum points for this slide
    fn max_points(&self) -> u64;

    /// Get the time limit for answers, or `None` for host-paced (no timer)
    fn time_limit(&self) -> Option<Duration>;
}

/// Helper function to add scores to leaderboard (common across all slide types)
pub(crate) fn add_scores_to_leaderboard<
    F: TunnelFinder,
    AnswerType: Clone,
    A: AnswerHandler<AnswerType>,
    T: SlideTimer,
>(
    slide: &A,
    timer: &T,
    leaderboard: &mut Leaderboard,
    watchers: &Watchers,
    team_manager: Option<&TeamManager<crate::names::NameStyle>>,
    tunnel_finder: F,
) {
    let starting_instant = timer.timer();

    leaderboard.add_scores(
        &slide
            .user_answers()
            .iter()
            .map(|(id, (answer, instant))| {
                let multiplier = slide.score_multiplier(answer);
                let time_score = calculate_slide_score(
                    slide.time_limit(),
                    instant.duration_since(starting_instant).unwrap_or_default(),
                    slide.max_points(),
                );
                (*id, (time_score as f64 * multiplier) as u64, instant)
            })
            .into_grouping_map_by(|(id, _, _)| {
                let player_id = *id;
                match &team_manager {
                    Some(team_manager) => team_manager.get_team(player_id).unwrap_or(player_id),
                    None => player_id,
                }
            })
            .min_by_key(|_, (_, _, instant)| *instant)
            .into_iter()
            .map(|(id, (_, score, _))| (id, score))
            .chain(
                {
                    match &team_manager {
                        Some(team_manager) => team_manager.all_ids(),
                        None => watchers
                            .specific_vec(ValueKind::Player, tunnel_finder)
                            .into_iter()
                            .map(|(x, _, _)| x)
                            .collect_vec(),
                    }
                }
                .into_iter()
                .map(|id| (id, 0)),
            )
            .unique_by(|(id, _)| *id)
            .collect_vec(),
    );
}

/// True when every currently-live player has answered.
///
/// O(1): reads counters maintained by the slide and the watcher map. Relies
/// on `reverse_mapping[Player]` reflecting only live watchers (see
/// [`Watchers::watcher_left`](crate::watcher::Watchers::watcher_left) and
/// [`Watchers::watcher_returned`](crate::watcher::Watchers::watcher_returned)).
pub fn all_players_answered<AnswerType, A: AnswerHandler<AnswerType>>(slide: &A, watchers: &Watchers) -> bool {
    let live_players = watchers.specific_count(ValueKind::Player);
    live_players > 0 && slide.live_answered_count() >= live_players
}

/// Number of live players who have answered. O(1).
pub fn get_answered_count<AnswerType, A: AnswerHandler<AnswerType>>(slide: &A) -> usize {
    slide.live_answered_count()
}

/// Common interface for all question types to handle incoming messages
///
/// This trait abstracts the message handling logic that is common across
/// all question types, allowing for uniform treatment of different slide types.
pub(crate) trait QuestionReceiveMessage {
    /// Handle host "Next" command
    ///
    /// This method processes the host's request to advance to the next phase
    /// or complete the slide.
    ///
    /// # Arguments
    ///
    /// * `leaderboard` - Mutable reference to the game leaderboard
    /// * `watchers` - Connection manager for all participants
    /// * `team_manager` - Optional team manager for team-based games
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
    ///
    /// # Returns
    ///
    /// A `SlideAction` indicating whether to stay on the current slide or advance
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    fn receive_host_next<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SlideAction<S>;

    /// Handle player messages
    ///
    /// This method processes player-specific messages like answer submissions
    /// and other player interactions.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the player sending the message
    /// * `message` - The player message to process
    /// * `watchers` - Connection manager for all participants
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn receive_player_message<F: TunnelFinder>(
        &mut self,
        watcher_id: Id,
        message: crate::game::IncomingPlayerMessage,
        watchers: &Watchers,
        tunnel_finder: F,
    );

    /// Combined message handler that delegates to specific handlers
    ///
    /// This method provides a unified interface for handling both host and player
    /// messages by delegating to the appropriate specific handler method.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the participant sending the message
    /// * `message` - The incoming message to process
    /// * `leaderboard` - Mutable reference to the game leaderboard
    /// * `watchers` - Connection manager for all participants
    /// * `team_manager` - Optional team manager for team-based games
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    /// * `index` - Current slide index in the game
    /// * `count` - Total number of slides in the game
    ///
    /// # Returns
    ///
    /// A `SlideAction` indicating whether to stay on the current slide or advance
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    fn receive_message<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watcher_id: Id,
        message: crate::game::IncomingMessage,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        match message {
            crate::game::IncomingMessage::Host(crate::game::IncomingHostMessage::Next) => self.receive_host_next(
                leaderboard,
                watchers,
                team_manager,
                schedule_message,
                tunnel_finder,
                index,
                count,
            ),
            crate::game::IncomingMessage::Player(player_message) => {
                self.receive_player_message(watcher_id, player_message, watchers, tunnel_finder);
                SlideAction::Stay
            }
            crate::game::IncomingMessage::Host(
                crate::game::IncomingHostMessage::Index(_) | crate::game::IncomingHostMessage::Lock(_),
            )
            | crate::game::IncomingMessage::Ghost(_)
            | crate::game::IncomingMessage::Unassigned(_) => SlideAction::Stay,
        }
    }
}
