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
use rustc_hash::{FxBuildHasher, FxHashMap};
use serde::{Deserialize, Serialize};

use crate::{
    fuiz::config::{ScheduleMessageFn, SlideAction},
    leaderboard::Leaderboard,
    session::TunnelFinder,
    teams::TeamManager,
    time::Timestamp,
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
    fn answer_start(&self) -> Option<Timestamp>;

    /// Set the answer start time, or None if not started
    fn set_answer_start(&mut self, time: Option<Timestamp>);

    /// Start the timer by setting the current time
    fn start_timer(&mut self) {
        self.set_answer_start(Some(Timestamp::now()));
    }

    /// Get the timer start time, or current time if not set
    fn timer(&self) -> Timestamp {
        self.answer_start().unwrap_or(Timestamp::now())
    }

    /// Get the elapsed time since the timer started
    fn elapsed(&self) -> Duration {
        self.timer().elapsed()
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
    fn user_answers(&self) -> &FxHashMap<Id, (AnswerType, Timestamp)>;

    /// Get mutable access to user answers with timestamps
    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (AnswerType, Timestamp)>;

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
    fn ids_of_who_answered(&self) -> Copied<Keys<'_, Id, (AnswerType, Timestamp)>> {
        self.user_answers().keys().copied()
    }

    /// Pre-allocates answer-map buckets for an upcoming round.
    ///
    /// Call when the slide transitions into the answer-accepting state so the
    /// map's growth from 0 → live-player-count doesn't trigger rawtable
    /// rehashes on the answer hot path.
    fn reserve_for_players(&mut self, live_player_count: usize) {
        self.user_answers_mut().reserve(live_player_count);
    }

    /// Records a player's answer with the current timestamp.
    ///
    /// Updates the live-answered counter when this is the first answer from
    /// `id` (so a player overwriting their own answer doesn't double-count).
    fn record_answer(&mut self, id: Id, answer: AnswerType) {
        let transformed_answer = self.transform_answer(answer);
        let was_new = self
            .user_answers_mut()
            .insert(id, (transformed_answer, Timestamp::now()))
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

    /// Wrap `count` in this slide-type's `AnswersCount` variant of the
    /// outer [`crate::UpdateMessage`] enum. Each impl is a one-liner —
    /// `UpdateMessage::AnswersCount(count).into()` — the trait machinery
    /// handles the fan-out.
    fn answers_count_message(count: usize) -> crate::UpdateMessage<'static>;

    /// Transition the slide into `AnswersResults` state and announce results.
    fn send_answers_results<F: TunnelFinder>(&mut self, watchers: &Watchers, tunnel_finder: F);

    /// Send the per-slide `AnswersCount` tick to the host using
    /// [`Self::answers_count_message`] for the per-type message construction.
    fn send_answers_count<F: TunnelFinder>(&self, count: usize, watchers: &Watchers, tunnel_finder: F)
    where
        Self: Sized,
    {
        watchers.announce_specific(ValueKind::Host, &Self::answers_count_message(count), tunnel_finder);
    }

    /// Default flow run after a player answer has been recorded: if everyone
    /// has now answered, finalize the slide; otherwise emit a throttled
    /// `AnswersCount` tick to the host.
    fn handle_post_answer<F: TunnelFinder>(&mut self, watchers: &Watchers, tunnel_finder: F)
    where
        Self: Sized,
    {
        if all_players_answered(self, watchers) {
            self.send_answers_results(watchers, tunnel_finder);
        } else {
            let count = get_answered_count(self);
            if should_announce_answered_count(count) {
                self.send_answers_count(count, watchers, tunnel_finder);
            }
        }
    }
}

/// Helper function to add scores to leaderboard (common across all slide types)
///
/// Walks every recorded answer once and folds it into a per-leaderboard-id map
/// keeping the earliest-instant winner per group (team, or player when there
/// are no teams). Replaces an earlier `itertools::into_grouping_map_by` form
/// whose internal `HashMap` used the default SipHash; the explicit `FxHashMap`
/// here removes ~5% of full-game CPU at 4000 players.
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

    let leaderboard_id = |player_id: Id| match &team_manager {
        Some(tm) => tm.get_team(player_id).unwrap_or(player_id),
        None => player_id,
    };

    // Pre-size to the answerer count: there's at most one map entry per
    // distinct group, and the answer set bounds that. Avoids 0→N rawtable
    // rehashes (the old default-sized map cost ~3% of full_game at 4000).
    let mut earliest_per_group: FxHashMap<Id, (u64, Timestamp)> =
        FxHashMap::with_capacity_and_hasher(slide.user_answers().len(), FxBuildHasher);
    for (id, (answer, instant)) in slide.user_answers() {
        let multiplier = slide.score_multiplier(answer);
        let time_score = calculate_slide_score(
            slide.time_limit(),
            instant.duration_since(starting_instant),
            slide.max_points(),
        );
        let score = (time_score as f64 * multiplier) as u64;
        let key = leaderboard_id(*id);
        earliest_per_group
            .entry(key)
            .and_modify(|(existing_score, existing_instant)| {
                if instant < existing_instant {
                    *existing_score = score;
                    *existing_instant = *instant;
                }
            })
            .or_insert((score, *instant));
    }

    let mut scores: Vec<(Id, u64)> = earliest_per_group
        .iter()
        .map(|(id, (score, _))| (*id, *score))
        .collect();

    let all_ids = match &team_manager {
        Some(team_manager) => team_manager.all_ids(),
        None => watchers
            .specific_iter(ValueKind::Player, tunnel_finder)
            .map(|(id, _, _)| id)
            .collect_vec(),
    };
    for id in all_ids {
        if !earliest_per_group.contains_key(&id) {
            scores.push((id, 0));
        }
    }

    leaderboard.add_scores(&scores);
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

/// True when the host should receive an `AnswersCount` tick for this `count`.
///
/// Logarithmic throttle: keeps ~5 bits of precision in `count`, so the host
/// sees every answer up to 31 and then progressively coarser updates (step ≈
/// 3-6% of current value). For a 4000-player lobby this drops ~4000 ticks per
/// slide to ~144 without losing perceptual smoothness on the progress bar.
pub fn should_announce_answered_count(count: usize) -> bool {
    const SIGNIFICANT_BITS: u32 = 5;
    count.leading_zeros() + count.trailing_zeros() >= usize::BITS - SIGNIFICANT_BITS
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

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::watcher::{PlayerValue, Value, Watchers};

    /// Minimal `AnswerHandler` for exercising the trait's defaulted methods.
    /// Uses `bool` as the answer type: `true` = correct, `false` = incorrect.
    /// `Cell` fields capture trait-method dispatch so `handle_post_answer` can
    /// be tested without a real `Watchers` fan-out.
    struct MockSlide {
        answers: FxHashMap<Id, (bool, Timestamp)>,
        live_answered: usize,
        time_limit: Option<Duration>,
        max_points: u64,
        last_count_tick: Cell<Option<usize>>,
        results_sent: Cell<bool>,
    }

    impl MockSlide {
        fn new() -> Self {
            Self {
                answers: FxHashMap::default(),
                live_answered: 0,
                time_limit: Some(Duration::from_secs(10)),
                max_points: 1000,
                last_count_tick: Cell::new(None),
                results_sent: Cell::new(false),
            }
        }
    }

    impl AnswerHandler<bool> for MockSlide {
        fn user_answers(&self) -> &FxHashMap<Id, (bool, Timestamp)> {
            &self.answers
        }
        fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (bool, Timestamp)> {
            &mut self.answers
        }
        fn live_answered_count(&self) -> usize {
            self.live_answered
        }
        fn live_answered_count_mut(&mut self) -> &mut usize {
            &mut self.live_answered
        }
        fn is_correct_answer(&self, answer: &bool) -> bool {
            *answer
        }
        fn max_points(&self) -> u64 {
            self.max_points
        }
        fn time_limit(&self) -> Option<Duration> {
            self.time_limit
        }
        fn answers_count_message(_count: usize) -> crate::UpdateMessage<'static> {
            // Unit-test shim: pick any existing variant so the type checks;
            // the test asserts on the dispatcher via `send_answers_count`
            // override below, not by inspecting the constructed message.
            crate::UpdateMessage::Game(crate::game::UpdateMessage::IdAssign(Id::new()))
        }
        fn send_answers_count<F: TunnelFinder>(&self, count: usize, _watchers: &Watchers, _tunnel_finder: F) {
            self.last_count_tick.set(Some(count));
        }
        fn send_answers_results<F: TunnelFinder>(&mut self, _watchers: &Watchers, _tunnel_finder: F) {
            self.results_sent.set(true);
        }
    }

    // ---------- should_announce_answered_count ----------

    #[test]
    fn small_counts_under_threshold_always_send() {
        for count in 0..32 {
            assert!(
                should_announce_answered_count(count),
                "count {count} should send (below 2^5 shortcut)"
            );
        }
    }

    #[test]
    fn boundary_between_bands_sends_or_skips_as_expected() {
        // Band 32..63 → step 2 (send on even).
        assert!(should_announce_answered_count(32));
        assert!(!should_announce_answered_count(33));
        assert!(should_announce_answered_count(34));
        assert!(!should_announce_answered_count(63));
        // Band 64..127 → step 4.
        assert!(should_announce_answered_count(64));
        assert!(!should_announce_answered_count(65));
        assert!(!should_announce_answered_count(66));
        assert!(should_announce_answered_count(68));
        // Band 128..255 → step 8.
        assert!(should_announce_answered_count(128));
        assert!(!should_announce_answered_count(129));
        assert!(should_announce_answered_count(136));
        assert!(!should_announce_answered_count(132));
    }

    #[test]
    fn larger_counts_follow_log_pattern() {
        // Band 1024..2047 → step 64.
        assert!(should_announce_answered_count(1024));
        assert!(should_announce_answered_count(1088)); // 1024 + 64
        assert!(!should_announce_answered_count(1065));
        // Band 2048..4095 → step 128.
        assert!(should_announce_answered_count(2048));
        assert!(should_announce_answered_count(2176)); // 2048 + 128
        assert!(!should_announce_answered_count(2050));
    }

    #[test]
    fn send_count_matches_log_total() {
        // For values 0..4096 the predicate should fire roughly log-many times.
        // Exact count: 32 (band 0..31) + 16 per band × 7 bands = 144.
        let sent: usize = (0..4096).filter(|&n| should_announce_answered_count(n)).count();
        assert_eq!(sent, 144);
    }

    // ---------- calculate_slide_score ----------

    #[test]
    fn score_is_full_in_host_paced_mode() {
        assert_eq!(calculate_slide_score(None, Duration::from_secs(0), 1000), 1000);
        assert_eq!(
            calculate_slide_score(None, Duration::from_secs(60), 1000),
            1000,
            "host-paced mode ignores how long the player took"
        );
    }

    #[test]
    fn instant_answer_earns_full_points() {
        let limit = Some(Duration::from_secs(10));
        assert_eq!(calculate_slide_score(limit, Duration::from_secs(0), 1000), 1000);
    }

    #[test]
    fn half_time_answer_earns_three_quarter_points() {
        // formula: pts * (1 - taken/full/2) → 1 - 0.5/2 = 0.75
        let limit = Some(Duration::from_secs(10));
        assert_eq!(calculate_slide_score(limit, Duration::from_secs(5), 1000), 750);
    }

    #[test]
    fn end_of_window_answer_earns_half_points() {
        // 1 - 1.0/2 = 0.5
        let limit = Some(Duration::from_secs(10));
        assert_eq!(calculate_slide_score(limit, Duration::from_secs(10), 1000), 500);
    }

    // ---------- AnswerHandler default methods ----------

    #[test]
    fn record_answer_increments_live_count_only_on_first_submission() {
        let mut slide = MockSlide::new();
        let id = Id::new();

        slide.record_answer(id, true);
        assert_eq!(slide.live_answered_count(), 1);
        assert!(slide.user_answers().contains_key(&id));

        // Second answer from the same player must not double-count.
        slide.record_answer(id, false);
        assert_eq!(slide.live_answered_count(), 1, "overwrite should not bump counter");
        assert_eq!(slide.user_answers().get(&id).map(|(a, _)| *a), Some(false));
    }

    #[test]
    fn mark_watcher_left_decrements_only_for_answered_players() {
        let mut slide = MockSlide::new();
        let answered = Id::new();
        let absent = Id::new();

        slide.record_answer(answered, true);
        assert_eq!(slide.live_answered_count(), 1);

        // Leaving without having answered is a no-op.
        slide.mark_watcher_left(absent);
        assert_eq!(slide.live_answered_count(), 1);

        // Leaving after answering decrements.
        slide.mark_watcher_left(answered);
        assert_eq!(slide.live_answered_count(), 0);

        // Saturating: another leave doesn't underflow.
        slide.mark_watcher_left(answered);
        assert_eq!(slide.live_answered_count(), 0);
    }

    #[test]
    fn mark_watcher_returned_only_credits_back_if_answer_is_on_file() {
        let mut slide = MockSlide::new();
        let answered = Id::new();
        let absent = Id::new();

        slide.record_answer(answered, true);
        slide.mark_watcher_left(answered);
        assert_eq!(slide.live_answered_count(), 0);

        // Returning a player who had answered restores the count.
        slide.mark_watcher_returned(answered);
        assert_eq!(slide.live_answered_count(), 1);

        // Returning a player who never answered is a no-op.
        slide.mark_watcher_returned(absent);
        assert_eq!(slide.live_answered_count(), 1);
    }

    #[test]
    fn correct_count_filters_via_is_correct_answer() {
        let mut slide = MockSlide::new();
        slide.record_answer(Id::new(), true);
        slide.record_answer(Id::new(), false);
        slide.record_answer(Id::new(), true);
        slide.record_answer(Id::new(), false);

        assert_eq!(slide.correct_count(), 2);
    }

    #[test]
    fn answer_counts_groups_by_answer_value() {
        let mut slide = MockSlide::new();
        slide.record_answer(Id::new(), true);
        slide.record_answer(Id::new(), true);
        slide.record_answer(Id::new(), true);
        slide.record_answer(Id::new(), false);

        let counts = slide.answer_counts();
        assert_eq!(counts.get(&true).copied(), Some(3));
        assert_eq!(counts.get(&false).copied(), Some(1));
    }

    #[test]
    fn ids_of_who_answered_returns_exactly_the_submitters() {
        let mut slide = MockSlide::new();
        let a = Id::new();
        let b = Id::new();

        slide.record_answer(a, true);
        slide.record_answer(b, false);

        let mut ids: Vec<_> = slide.ids_of_who_answered().collect();
        ids.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(ids, want);
    }

    #[test]
    fn score_multiplier_defaults_to_binary_correctness() {
        let slide = MockSlide::new();
        assert!((slide.score_multiplier(&true) - 1.0).abs() < f64::EPSILON);
        assert!((slide.score_multiplier(&false) - 0.0).abs() < f64::EPSILON);
    }

    // ---------- all_players_answered / get_answered_count ----------

    fn populate_watchers(count: usize) -> Watchers {
        let mut watchers = Watchers::new(count.max(1));
        for _ in 0..count {
            watchers
                .add_watcher(Id::new(), Value::Player(PlayerValue::Individual))
                .expect("under capacity");
        }
        watchers
    }

    #[test]
    fn all_players_answered_is_false_with_no_players() {
        let slide = MockSlide::new();
        let watchers = Watchers::new(10);
        assert!(!all_players_answered(&slide, &watchers));
    }

    #[test]
    fn all_players_answered_is_false_when_some_havent_submitted() {
        let mut slide = MockSlide::new();
        let watchers = populate_watchers(3);

        slide.record_answer(Id::new(), true); // not in watchers, just increments counter
        slide.record_answer(Id::new(), true);

        assert!(!all_players_answered(&slide, &watchers));
    }

    #[test]
    fn all_players_answered_is_true_once_count_meets_live_player_total() {
        let mut slide = MockSlide::new();
        let watchers = populate_watchers(3);

        for _ in 0..3 {
            slide.record_answer(Id::new(), true);
        }

        assert!(all_players_answered(&slide, &watchers));
    }

    #[test]
    fn get_answered_count_mirrors_live_answered_count() {
        let mut slide = MockSlide::new();
        assert_eq!(get_answered_count(&slide), 0);

        slide.record_answer(Id::new(), true);
        slide.record_answer(Id::new(), false);
        assert_eq!(get_answered_count(&slide), 2);
    }

    // ---------- handle_post_answer (the consolidated post-answer flow) ----------

    fn noop_tunnel_finder() -> impl Fn(Id) -> Option<NoopTunnel> + Copy {
        |_| None
    }

    #[derive(Clone, Default)]
    struct NoopTunnel;
    impl crate::session::Tunnel for NoopTunnel {
        fn send_message(&self, _message: &crate::UpdateMessage) {}
        fn send_state(&self, _state: &crate::SyncMessage) {}
        fn close(self) {}
    }

    #[test]
    fn handle_post_answer_finalizes_when_all_have_answered() {
        let mut slide = MockSlide::new();
        let watchers = populate_watchers(3);

        for _ in 0..3 {
            slide.record_answer(Id::new(), true);
        }

        slide.handle_post_answer(&watchers, noop_tunnel_finder());

        assert!(slide.results_sent.get(), "results should fire when everyone answered");
        assert_eq!(
            slide.last_count_tick.get(),
            None,
            "count tick must not fire on completion"
        );
    }

    #[test]
    fn handle_post_answer_ticks_on_throttle_boundary() {
        let mut slide = MockSlide::new();
        let watchers = populate_watchers(100);

        // Drive the live-answered count to 32 — the first boundary above the
        // small-value shortcut, so the throttle should fire here.
        slide.live_answered = 32;

        slide.handle_post_answer(&watchers, noop_tunnel_finder());

        assert!(!slide.results_sent.get());
        assert_eq!(slide.last_count_tick.get(), Some(32));
    }

    #[test]
    fn handle_post_answer_skips_tick_between_boundaries() {
        let mut slide = MockSlide::new();
        let watchers = populate_watchers(100);

        // 33 falls inside the 32..63 band (step 2) but isn't a multiple of 2.
        slide.live_answered = 33;

        slide.handle_post_answer(&watchers, noop_tunnel_finder());

        assert!(!slide.results_sent.get());
        assert_eq!(slide.last_count_tick.get(), None, "33 is between throttle ticks");
    }
}
