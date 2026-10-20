//! Brainstorm (collect ideas, then vote on them) question implementation
//!
//! A brainstorm runs in two acts. First every player contributes ideas, which
//! land on a shared board and the host watches them arrive live. Then the board
//! is opened for voting and each player spends a small budget of votes on the
//! ideas they like best. The results rank the board by votes.
//!
//! Brainstorms collect opinions, so nothing is correct and no points are
//! awarded; the slide still records a zero for every player so per-slide point
//! arrays stay aligned with slide indices.

use std::time::Duration;

use garde::Validate;
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHashSet};
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
        SlideTimer, all_players_answered, get_answered_count, impl_slide_core, should_announce_answered_count,
    },
    media::Media,
};

/// Lifecycle phases for a brainstorm slide.
#[derive(Copy, Clone, Eq, PartialEq, Debug, Default, Serialize, Deserialize)]
#[repr(u8)]
pub enum Phase {
    /// Initial state before the slide has started.
    #[default]
    Unstarted,
    /// Displaying the prompt without the idea input.
    Question,
    /// Collecting ideas from players.
    Ideas,
    /// Voting on the collected ideas.
    Voting,
    /// Displaying the board ranked by votes.
    AnswersResults,
}

impl super::common::Phase for Phase {
    fn next(self) -> Option<Self> {
        match self {
            Self::Unstarted => Some(Self::Question),
            Self::Question => Some(Self::Ideas),
            Self::Ideas => Some(Self::Voting),
            Self::Voting => Some(Self::AnswersResults),
            Self::AnswersResults => None,
        }
    }
}

/// Configuration for a brainstorm slide
#[derive(Debug, Clone, Serialize, Deserialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
pub struct SlideConfig {
    /// The prompt players brainstorm against
    #[garde(length(chars, min = ctx.question.min_title_length, max = ctx.question.max_title_length))]
    title: String,
    /// Accompanying media
    #[garde(dive)]
    media: Option<Media>,
    /// Duration of the slide-announcement intro shown before the prompt.
    /// Absent means a default duration; `null` means host-paced.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_slide(val)))]
    #[serde(
        default = "crate::fuiz::common::default_introduce_slide",
        with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>"
    )]
    introduce_slide: Option<Duration>,
    /// Time before the idea input is displayed.
    /// `None` means host-paced: the host must manually advance.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_introduce_question(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    introduce_question: Option<Duration>,
    /// Time where players can contribute ideas.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    idea_time_limit: Option<Duration>,
    /// Time where players can vote on the board.
    /// `None` means host-paced: no timer, host advances manually.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_time_limit(val)))]
    #[serde(default, with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    vote_time_limit: Option<Duration>,
    /// Points awarded. Brainstorms collect opinions, so this is `0` in
    /// practice; the field exists so every slide type announces the same way.
    #[garde(skip)]
    #[serde(default)]
    points_awarded: u64,
    /// How many ideas a single player may contribute
    #[garde(range(min = 1, max = ctx.brainstorm.max_ideas_per_player))]
    #[serde(default = "default_ideas_per_player")]
    max_ideas_per_player: usize,
    /// How many votes a single player may cast
    #[garde(range(min = 1, max = ctx.brainstorm.max_votes_per_player))]
    #[serde(default = "default_votes_per_player")]
    max_votes_per_player: usize,
    /// Maximum length of an idea in characters
    #[garde(range(min = 1, max = ctx.brainstorm.max_idea_length))]
    #[serde(default = "default_idea_length")]
    max_idea_length: usize,
}

fn default_ideas_per_player() -> usize {
    crate::settings::BrainstormSettings::default().max_ideas_per_player
}

fn default_votes_per_player() -> usize {
    crate::settings::BrainstormSettings::default().max_votes_per_player
}

fn default_idea_length() -> usize {
    crate::settings::BrainstormSettings::default().max_idea_length
}

/// How many ideas the shared board holds before it stops accepting new ones.
const MAX_IDEAS_TOTAL: usize = 200;

/// Runtime state for a brainstorm slide
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub struct State {
    /// The configuration this state was created from
    config: SlideConfig,

    // Runtime State
    /// The shared board, in the order ideas arrived
    ideas: Vec<String>,
    /// Lowercased ideas already on the board, so near-duplicates don't pile up
    seen_ideas: FxHashSet<String>,
    /// Players who have contributed at least one idea
    idea_submitters: FxHashSet<Id>,
    /// The idea indices each player voted for, with submission timestamps
    user_answers: FxHashMap<Id, (Vec<usize>, Timestamp)>,
    /// Shared runtime core: slide phase, answer-start timestamp, live-answered tally.
    ///
    /// The tally counts idea submitters during [`Phase::Ideas`] and voters
    /// during [`Phase::Voting`]; it is reset when the phases change over.
    #[cfg_attr(feature = "serializable", serde(flatten))]
    core: SlideCore<Phase>,
}

impl SlideConfig {
    /// Creates a new runtime state from this configuration
    pub fn to_state(&self) -> State {
        State {
            config: self.clone(),
            ideas: Vec::new(),
            seen_ideas: FxHashSet::default(),
            idea_submitters: FxHashSet::default(),
            user_answers: FxHashMap::default(),
            core: SlideCore::default(),
        }
    }
}

/// One idea on the board and how many votes it drew.
#[derive(Debug, Clone, Serialize)]
pub struct IdeaResult {
    /// The idea as it was contributed.
    pub text: String,
    /// How many votes it drew.
    pub votes: usize,
}

/// Aggregated results for a brainstorm slide.
#[derive(Debug, Clone, Serialize)]
pub struct Results {
    /// The board ranked by votes, ties broken by the order ideas arrived.
    pub ideas: Vec<IdeaResult>,
    /// Number of players who cast at least one vote.
    pub voter_count: usize,
    /// Number of players who contributed at least one idea.
    pub contributor_count: usize,
}

/// Messages sent to listeners to update their pre-existing brainstorm state.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Announces the upcoming slide's type and scoring (the `Unstarted` phase).
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
    },
    /// Announces the prompt without the idea input
    QuestionAnnouncement {
        /// Index of the current slide (0-based)
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The prompt being brainstormed
        question: &'a str,
        /// Optional media content accompanying the prompt
        media: Option<&'a Media>,
        /// Time before the idea input appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Opens the idea-collection window
    IdeasAnnouncement {
        /// Time before idea collection closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// How many ideas a single player may contribute
        max_ideas: usize,
        /// Maximum length of an idea
        max_idea_length: usize,
    },
    /// (HOST ONLY) A new idea landed on the board
    IdeaAdded(&'a str),
    /// (HOST ONLY) Reports the number of players who have submitted or voted
    AnswersCount(usize),
    /// Opens voting on the collected board
    VotingAnnouncement {
        /// Time before voting closes, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The board, in the order ideas arrived
        ideas: Vec<&'a str>,
        /// How many votes a single player may cast
        max_votes: usize,
    },
    /// Shows the board ranked by votes
    AnswersResults {
        /// Aggregated votes
        results: Results,
    },
}

/// Scheduled phase-transition alarm for brainstorm slides.
pub type AlarmMessage = ProceedFromSlideIntoSlide<Phase>;

/// Messages sent to listeners who lack pre-existing brainstorm state.
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
    },
    /// Synchronizes the prompt announcement phase
    QuestionAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The prompt being brainstormed
        question: &'a str,
        /// Optional media content accompanying the prompt
        media: Option<&'a Media>,
        /// Remaining time before the idea input appears, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
    },
    /// Synchronizes the idea-collection phase
    IdeasAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The prompt being brainstormed
        question: &'a str,
        /// Optional media content accompanying the prompt
        media: Option<&'a Media>,
        /// Remaining collection time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The board so far, in the order ideas arrived
        ideas: Vec<&'a str>,
        /// How many ideas a single player may contribute
        max_ideas: usize,
        /// Maximum length of an idea
        max_idea_length: usize,
        /// Number of players who have already contributed
        answered_count: usize,
    },
    /// Synchronizes the voting phase
    VotingAnnouncement {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The prompt being brainstormed
        question: &'a str,
        /// Optional media content accompanying the prompt
        media: Option<&'a Media>,
        /// Remaining voting time, or `None` for host-paced
        #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
        duration: Option<Duration>,
        /// The board, in the order ideas arrived
        ideas: Vec<&'a str>,
        /// How many votes a single player may cast
        max_votes: usize,
        /// Number of players who have already voted
        answered_count: usize,
    },
    /// Synchronizes the results phase
    AnswersResults {
        /// Index of the current slide
        index: usize,
        /// Total number of slides in the game
        count: usize,
        /// The prompt that was brainstormed
        question: &'a str,
        /// Optional media content that accompanied the prompt
        media: Option<&'a Media>,
        /// Aggregated votes
        results: Results,
    },
}

impl_slide_core!(State, Phase);

impl AnswerHandler<Vec<usize>> for State {
    fn user_answers(&self) -> &FxHashMap<Id, (Vec<usize>, Timestamp)> {
        &self.user_answers
    }

    fn user_answers_mut(&mut self) -> &mut FxHashMap<Id, (Vec<usize>, Timestamp)> {
        &mut self.user_answers
    }

    /// Opinions have no right answer, so nothing scores.
    fn is_correct_answer(&self, _answer: &Vec<usize>) -> bool {
        false
    }

    fn describe_answer(&self, answer: &Vec<usize>) -> String {
        // Votes are the answer here; the ideas themselves are already on screen.
        answer
            .iter()
            .filter_map(|index| self.ideas.get(*index).cloned())
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn max_points(&self) -> u64 {
        self.config.points_awarded
    }

    fn time_limit(&self) -> Option<Duration> {
        self.config.vote_time_limit
    }

    fn answers_count_message(count: usize) -> crate::UpdateMessage<'static> {
        UpdateMessage::AnswersCount(count).into()
    }

    /// During [`Phase::Ideas`] the live tally counts contributors rather than
    /// voters, so leaving mid-collection has to release the right seat.
    fn mark_watcher_left(&mut self, id: Id) {
        let counted = match self.state() {
            Phase::Ideas => self.idea_submitters.contains(&id),
            _ => self.user_answers.contains_key(&id),
        };
        if counted {
            let counter = self.live_answered_count_mut();
            *counter = counter.saturating_sub(1);
        }
    }

    /// Mirror of [`Self::mark_watcher_left`] for a reconnecting player.
    fn mark_watcher_returned(&mut self, id: Id) {
        let counted = match self.state() {
            Phase::Ideas => self.idea_submitters.contains(&id),
            _ => self.user_answers.contains_key(&id),
        };
        if counted {
            *self.live_answered_count_mut() += 1;
        }
    }

    fn send_answers_results<F: TunnelFinder>(&mut self, watchers: &Watchers, tunnel_finder: F) {
        if self.change_state(Phase::Voting, Phase::AnswersResults) {
            watchers.announce(
                &UpdateMessage::AnswersResults {
                    results: self.results(),
                }
                .into(),
                tunnel_finder,
            );
        }
    }
}

impl PhasedSlide<Vec<usize>> for State {
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
            Phase::Unstarted => self.announce_slide(watchers, schedule_message, tick, tunnel_finder, index, count),
            Phase::Question => self.enter_question(watchers, schedule_message, tick, tunnel_finder, index, count),
            Phase::Ideas => self.enter_ideas(watchers, schedule_message, tick, tunnel_finder, index),
            Phase::Voting => self.enter_voting(watchers, schedule_message, tick, tunnel_finder, index),
            Phase::AnswersResults => self.send_answers_results(watchers, tunnel_finder),
        }
    }
}

impl State {
    /// Shows the prompt, then opens idea collection after `introduce_question`.
    fn enter_question<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) {
        if !self.change_state(Phase::Unstarted, Phase::Question) {
            return;
        }

        self.start_timer_at(tick.now());

        // Announced before any skip ahead. This is the only live message
        // carrying the prompt and its media, and the one that opens idea
        // collection carries neither, so a zero-length intro must not be
        // allowed to swallow it.
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

        if let Some(duration) = self.config.introduce_question
            && duration.is_zero()
        {
            self.enter_ideas(watchers, schedule_message, tick, tunnel_finder, index);
            return;
        }

        if let Some(duration) = self.config.introduce_question {
            schedule_message(
                AlarmMessage {
                    index,
                    to: Phase::Ideas,
                }
                .into(),
                duration,
            );
        }
    }

    /// Opens the board for contributions.
    fn enter_ideas<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
    ) {
        if !self.change_state(Phase::Question, Phase::Ideas) {
            return;
        }
        self.start_timer_at(tick.now());
        // In this phase the live tally counts contributors, not voters.
        self.core.live_answered_count = 0;

        watchers.announce(
            &UpdateMessage::IdeasAnnouncement {
                duration: self.config.idea_time_limit,
                max_ideas: self.config.max_ideas_per_player,
                max_idea_length: self.config.max_idea_length,
            }
            .into(),
            tunnel_finder,
        );

        if let Some(time_limit) = self.config.idea_time_limit {
            schedule_message(
                AlarmMessage {
                    index,
                    to: Phase::Voting,
                }
                .into(),
                time_limit,
            );
        }
    }

    /// Opens the collected board for voting, or skips straight to the (empty)
    /// results when nobody contributed anything.
    fn enter_voting<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watchers: &Watchers,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
    ) {
        if !self.change_state(Phase::Ideas, Phase::Voting) {
            return;
        }
        if self.ideas.is_empty() {
            self.send_answers_results(watchers, tunnel_finder);
            return;
        }

        self.start_timer_at(tick.now());
        // Back to counting voters.
        self.core.live_answered_count = 0;
        self.reserve_for_players(watchers.specific_count(ValueKind::Player));

        watchers.announce(
            &UpdateMessage::VotingAnnouncement {
                duration: self.config.vote_time_limit,
                ideas: self.ideas.iter().map(String::as_str).collect_vec(),
                max_votes: self.config.max_votes_per_player,
            }
            .into(),
            tunnel_finder,
        );

        if let Some(time_limit) = self.config.vote_time_limit {
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
}

impl State {
    /// Announces the upcoming slide's type (the `Unstarted` phase), then
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

    /// Adds a player's ideas to the board, returning the newly accepted ones.
    ///
    /// Ideas are matched case-insensitively so "Recycling" and "recycling"
    /// don't split the vote, but the board keeps the first spelling it saw.
    fn accept_ideas(&mut self, ideas: Vec<String>) -> Vec<usize> {
        let mut added = Vec::new();
        for idea in ideas.into_iter().take(self.config.max_ideas_per_player) {
            if self.ideas.len() >= MAX_IDEAS_TOTAL {
                break;
            }
            let trimmed: String = idea.trim().chars().take(self.config.max_idea_length).collect();
            if trimmed.is_empty() {
                continue;
            }
            if self.seen_ideas.insert(trimmed.to_lowercase()) {
                self.ideas.push(trimmed);
                added.push(self.ideas.len() - 1);
            }
        }
        added
    }

    /// Ranks the board by votes, breaking ties by arrival order.
    fn results(&self) -> Results {
        let mut votes = vec![0_usize; self.ideas.len()];
        for (choices, _) in self.user_answers.values() {
            for &choice in choices {
                if let Some(slot) = votes.get_mut(choice) {
                    *slot += 1;
                }
            }
        }

        Results {
            ideas: self
                .ideas
                .iter()
                .enumerate()
                .map(|(i, text)| (i, text, votes[i]))
                .sorted_by(|(a_index, _, a_votes), (b_index, _, b_votes)| {
                    b_votes.cmp(a_votes).then_with(|| a_index.cmp(b_index))
                })
                .map(|(_, text, votes)| IdeaResult {
                    text: text.clone(),
                    votes,
                })
                .collect_vec(),
            voter_count: self.user_answers.len(),
            contributor_count: self.idea_submitters.len(),
        }
    }

    /// Starts the brainstorm slide by entering the [`Phase::Unstarted`] phase.
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
            Phase::Ideas => SyncMessage::IdeasAnnouncement {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                duration: self
                    .config
                    .idea_time_limit
                    .map(|duration| duration.saturating_sub(self.elapsed())),
                ideas: self.ideas.iter().map(String::as_str).collect_vec(),
                max_ideas: self.config.max_ideas_per_player,
                max_idea_length: self.config.max_idea_length,
                answered_count: get_answered_count(self),
            },
            Phase::Voting => SyncMessage::VotingAnnouncement {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
                duration: self
                    .config
                    .vote_time_limit
                    .map(|duration| duration.saturating_sub(self.elapsed())),
                ideas: self.ideas.iter().map(String::as_str).collect_vec(),
                max_votes: self.config.max_votes_per_player,
                answered_count: get_answered_count(self),
            },
            Phase::AnswersResults => SyncMessage::AnswersResults {
                index,
                count,
                question: &self.config.title,
                media: self.config.media.as_ref(),
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
        if let crate::AlarmMessage::Brainstorm(inner) = message {
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

    /// Records a player's ideas and mirrors the new ones to the host's board.
    ///
    /// Returns `true` once every live player has contributed, which is the
    /// caller's cue to close collection early.
    fn receive_ideas<F: TunnelFinder>(
        &mut self,
        watcher_id: Id,
        ideas: Vec<String>,
        watchers: &Watchers,
        tunnel_finder: F,
    ) -> bool {
        let added = self.accept_ideas(ideas);
        if added.is_empty() && !self.idea_submitters.contains(&watcher_id) {
            // Nothing usable arrived, so the player still owes the room an idea.
            return false;
        }

        for index in added {
            watchers.announce_specific(
                ValueKind::Host,
                &UpdateMessage::IdeaAdded(&self.ideas[index]).into(),
                &tunnel_finder,
            );
        }

        if self.idea_submitters.insert(watcher_id) {
            self.core.live_answered_count += 1;
        }

        if all_players_answered(self, watchers) {
            return true;
        }

        let count = get_answered_count(self);
        if should_announce_answered_count(count) {
            self.send_answers_count(count, watchers, tunnel_finder);
        }
        false
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
        // The caller here has no scheduler, so an early close of the idea phase
        // is left to `receive_message` (which does) or to the idea timer.
        let _ = self.handle_player_message(watcher_id, message, watchers, tick, tunnel_finder);
    }

    /// Overridden so a player message can close the idea phase early: moving
    /// into voting needs a scheduler for the vote timer, which the plain
    /// [`Self::receive_player_message`] hook doesn't get.
    fn receive_message<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watcher_id: Id,
        message: crate::game::IncomingMessage,
        leaderboard: &mut Leaderboard,
        watchers: &Watchers,
        team_manager: Option<&TeamManager<crate::names::NameStyle>>,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
        index: usize,
        count: usize,
    ) -> SlideAction<S> {
        match message {
            crate::game::IncomingMessage::Host(crate::game::IncomingHostMessage::Next(_)) => self.receive_host_next(
                leaderboard,
                watchers,
                team_manager,
                schedule_message,
                tick,
                tunnel_finder,
                index,
                count,
            ),
            crate::game::IncomingMessage::Player(player_message) => {
                if self.handle_player_message(watcher_id, player_message, watchers, tick, &tunnel_finder) {
                    self.enter_phase(
                        Phase::Voting,
                        team_manager,
                        watchers,
                        schedule_message,
                        tick,
                        tunnel_finder,
                        index,
                        count,
                    );
                }
                SlideAction::Stay
            }
            _ => SlideAction::Stay,
        }
    }
}

impl State {
    /// Routes a player message to the phase that cares about it.
    ///
    /// Returns `true` when every live player has now contributed an idea, so
    /// the caller can close collection without waiting for the timer.
    fn handle_player_message<F: TunnelFinder>(
        &mut self,
        watcher_id: Id,
        message: IncomingPlayerMessage,
        watchers: &Watchers,
        tick: Tick,
        tunnel_finder: F,
    ) -> bool {
        match (self.state(), message) {
            (Phase::Ideas, IncomingPlayerMessage::StringArrayAnswer(ideas)) => {
                self.receive_ideas(watcher_id, ideas, watchers, tunnel_finder)
            }
            (Phase::Ideas, IncomingPlayerMessage::StringAnswer(idea)) => {
                self.receive_ideas(watcher_id, vec![idea], watchers, tunnel_finder)
            }
            (Phase::Voting, IncomingPlayerMessage::IndexArrayAnswer(choices)) => {
                let choices = choices
                    .into_iter()
                    .filter(|&choice| choice < self.ideas.len())
                    .unique()
                    .take(self.config.max_votes_per_player)
                    .collect_vec();
                if !choices.is_empty() {
                    self.record_answer_at(watcher_id, choices, tick.now());
                    self.handle_post_answer(watchers, &tunnel_finder);
                }
                false
            }
            (Phase::Voting, IncomingPlayerMessage::IndexAnswer(choice)) => {
                if choice < self.ideas.len() {
                    self.record_answer_at(watcher_id, vec![choice], tick.now());
                    self.handle_post_answer(watchers, &tunnel_finder);
                }
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;

    fn config() -> SlideConfig {
        SlideConfig {
            title: "How do we cut waste?".to_string(),
            media: None,
            introduce_slide: None,
            introduce_question: None,
            idea_time_limit: None,
            vote_time_limit: None,
            points_awarded: 0,
            max_ideas_per_player: 3,
            max_votes_per_player: 2,
            max_idea_length: 200,
        }
    }

    #[test]
    fn ideas_are_deduplicated_case_insensitively() {
        let mut state = config().to_state();
        assert_eq!(state.accept_ideas(vec!["Recycling".to_string()]), vec![0]);
        assert_eq!(state.accept_ideas(vec!["  recycling ".to_string()]), [] as [usize; 0]);
        assert_eq!(state.ideas, vec!["Recycling".to_string()]);
    }

    #[test]
    fn blank_ideas_are_ignored() {
        let mut state = config().to_state();
        assert_eq!(state.accept_ideas(vec!["   ".to_string()]), [] as [usize; 0]);
        assert_eq!(state.ideas, [] as [String; 0]);
    }

    #[test]
    fn a_player_cannot_flood_the_board() {
        let mut state = config().to_state();
        let added = state.accept_ideas(vec![
            "one".to_string(),
            "two".to_string(),
            "three".to_string(),
            "four".to_string(),
        ]);
        assert_eq!(added.len(), 3, "max_ideas_per_player caps a single submission");
    }

    #[test]
    fn overlong_ideas_are_truncated() {
        let mut state = config().to_state();
        state.config.max_idea_length = 4;
        state.accept_ideas(vec!["abcdefgh".to_string()]);
        assert_eq!(state.ideas, vec!["abcd".to_string()]);
    }

    #[test]
    fn results_rank_by_votes_then_arrival() {
        let mut state = config().to_state();
        state.accept_ideas(vec!["first".to_string(), "second".to_string(), "third".to_string()]);
        state.record_answer(Id::new(), vec![2]);
        state.record_answer(Id::new(), vec![2, 0]);
        state.record_answer(Id::new(), vec![0]);

        let results = state.results();
        assert_eq!(
            results
                .ideas
                .iter()
                .map(|i| (i.text.as_str(), i.votes))
                .collect::<Vec<_>>(),
            vec![("first", 2), ("third", 2), ("second", 0)],
            "equal vote counts keep the order they were contributed in"
        );
        assert_eq!(results.voter_count, 3);
    }

    #[test]
    fn results_are_empty_when_nobody_contributed() {
        let state = config().to_state();
        let results = state.results();
        assert!(results.ideas.is_empty());
        assert_eq!(results.contributor_count, 0);
    }

    #[test]
    fn brainstorms_never_score() {
        let state = config().to_state();
        assert!(!state.is_correct_answer(&vec![0]));
        assert_eq!(state.max_points(), 0);
    }
}
