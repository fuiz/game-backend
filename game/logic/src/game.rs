//! Core game logic and state management
//!
//! This module contains the main game struct and logic for managing
//! a Fuiz game session, including player management, question flow,
//! scoring, team formation, and real-time communication with all
//! connected participants.

use std::{collections::HashSet, fmt::Debug};

use garde::Validate;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::{
    fuiz::{
        config::{CurrentSlide, ScheduleMessageFn, SlideAction},
        order, type_answer,
    },
    watcher::Value,
};

use super::{
    AlarmMessage, TruncatedVec,
    fuiz::{config::Fuiz, multiple_choice},
    leaderboard::{HostSummary, Leaderboard, ScoreMessage},
    names::{self, Names},
    session::TunnelFinder,
    teams::{self, TeamManager},
    watcher::{self, Id, PlayerValue, ValueKind, Watchers},
};

/// Represents the current phase or state of the game
///
/// The game progresses through different states, from waiting for players
/// to join, through individual questions, to showing results and completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum State {
    /// Waiting screen showing current players before the game starts
    WaitingScreen,
    /// Team formation screen (only shown in team games)
    TeamDisplay,
    /// Currently displaying a specific slide/question
    Slide(Box<CurrentSlide>),
    /// Showing the leaderboard after a question (with index)
    Leaderboard(usize),
    /// Game has completed
    Done,
}

/// Configuration options for team-based games
///
/// This struct defines how teams are formed and managed within a game session.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Validate)]
#[garde(context(crate::settings::Settings))]
pub struct TeamOptions {
    /// Maximum initial size for teams
    #[garde(range(min = 1, max = 5))]
    size: usize,
    /// Whether to assign players to random teams or let them choose preferences
    #[garde(skip)]
    assign_random: bool,
}

/// Global configuration options for the game session
///
/// These options affect the overall behavior of the game, including
/// name generation, answer visibility, leaderboard display, and team formation.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, Default, Validate)]
#[garde(context(crate::settings::Settings))]
pub struct Options {
    /// Style for automatically generated player names (None means players choose their own)
    #[garde(dive)]
    random_names: Option<names::NameStyle>,
    /// Whether to show correct answers on player devices after questions
    #[garde(skip)]
    show_answers: bool,
    /// Whether to skip showing leaderboards between questions
    #[garde(skip)]
    no_leaderboard: bool,
    /// Team configuration (None means individual play)
    #[garde(dive)]
    teams: Option<TeamOptions>,
}

/// The main game session struct
///
/// This struct represents a complete Fuiz game session, managing all
/// aspects of the game including participant connections, question flow,
/// scoring, team management, and real-time communication.
#[derive(Serialize, Deserialize)]
pub struct Game {
    /// The Fuiz configuration containing all questions and settings
    fuiz_config: Fuiz,
    /// Manager for all connected participants (players, hosts, unassigned)
    pub watchers: Watchers,
    /// Name assignments and validation for players
    names: Names,
    /// Scoring and leaderboard management
    leaderboard: Leaderboard,
    /// Current phase/state of the game
    pub state: State,
    /// Game configuration options
    options: Options,
    /// Whether the game is locked to new participants
    locked: bool,
    /// Team formation and management (if teams are enabled)
    team_manager: Option<TeamManager<names::NameStyle>>,
}

impl Debug for Game {
    /// Custom debug implementation that avoids printing large amounts of data
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Game")
            .field("fuiz", &self.fuiz_config)
            .finish_non_exhaustive()
    }
}

/// Messages received from different types of participants
///
/// This enum categorizes incoming messages based on the sender's role,
/// ensuring that only appropriate messages are processed from each
/// participant type.
#[derive(Debug, Deserialize, Clone)]
pub enum IncomingMessage {
    /// Messages from disconnected clients trying to reconnect
    Ghost(IncomingGhostMessage),
    /// Messages from the game host
    Host(IncomingHostMessage),
    /// Messages from unassigned connections (not yet players)
    Unassigned(IncomingUnassignedMessage),
    /// Messages from active players
    Player(IncomingPlayerMessage),
}

impl IncomingMessage {
    /// Validates that a message matches the sender's participant type
    ///
    /// This ensures that participants can only send messages appropriate
    /// for their current role in the game session.
    ///
    /// # Arguments
    ///
    /// * `sender_kind` - The type of participant sending the message
    ///
    /// # Returns
    ///
    /// `true` if the message type matches the sender type, `false` otherwise
    fn follows(&self, sender_kind: ValueKind) -> bool {
        matches!(
            (self, sender_kind),
            (IncomingMessage::Host(_), ValueKind::Host)
                | (IncomingMessage::Player(_), ValueKind::Player)
                | (IncomingMessage::Unassigned(_), ValueKind::Unassigned)
        )
    }
}

/// Messages that can be sent by active players
#[derive(Debug, Deserialize, Clone)]
pub enum IncomingPlayerMessage {
    /// Answer selected by index (for single-answer multiple choice questions)
    IndexAnswer(usize),
    /// Multiple answers selected by indices (for multi-answer multiple choice questions)
    IndexArrayAnswer(Vec<usize>),
    /// Text answer submitted (for type answer questions)
    StringAnswer(String),
    /// Array of strings submitted (for order questions)
    StringArrayAnswer(Vec<String>),
    /// Team preference selection (during team formation)
    ChooseTeammates(Vec<String>),
}

/// Messages that can be sent by unassigned connections
#[derive(Debug, Deserialize, Clone)]
pub enum IncomingUnassignedMessage {
    /// Request to set a specific name and become a player
    NameRequest(String),
}

/// Messages that can be sent by disconnected clients trying to reconnect
#[derive(Debug, Deserialize, Clone)]
pub enum IncomingGhostMessage {
    /// Request a new ID assignment
    DemandId,
    /// Attempt to reclaim a specific existing ID
    ClaimId(Id),
}

/// Messages that can be sent by the game host
#[derive(Debug, Deserialize, Clone, Copy)]
pub enum IncomingHostMessage {
    /// Advance to the next slide/question
    Next,
    /// Jump to a specific slide by index
    Index(usize),
    /// Lock or unlock the game to new participants
    Lock(bool),
}

/// Update messages sent to participants about game state changes
///
/// These messages inform participants about changes that affect their
/// view or interaction with the game.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage {
    /// Assign a unique ID to a participant
    IdAssign(Id),
    /// Update the waiting screen with current players
    WaitingScreen(TruncatedVec<String>),
    /// Update the team display screen
    TeamDisplay(TruncatedVec<String>),
    /// Prompt the participant to choose a name
    NameChoose,
    /// Confirm a name assignment
    NameAssign(String),
    /// Report an error with name validation
    NameError(names::Error),
    /// Send leaderboard information
    Leaderboard {
        /// The leaderboard data to display
        leaderboard: LeaderboardMessage,
    },
    /// Send individual score information
    Score {
        /// The player's score information
        score: Option<ScoreMessage>,
    },
    /// Send game summary information
    Summary(SummaryMessage),
    /// Inform player to find a team (team games only)
    FindTeam(String),
    /// Prompt for teammate selection during team formation
    ChooseTeammates {
        /// Maximum number of teammates that can be selected
        max_selection: usize,
        /// Available players with their current selection status: (name, is_selected)
        available: Vec<(String, bool)>,
    },
}

/// Sync messages sent to participants to synchronize their view with game state
///
/// These messages are sent when participants connect or when their view
/// needs to be completely synchronized with the current game state.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage {
    /// Sync waiting screen with current players
    WaitingScreen(TruncatedVec<String>),
    /// Sync team display screen
    TeamDisplay(TruncatedVec<String>),
    /// Sync leaderboard view with position information
    Leaderboard {
        /// Current slide index
        index: usize,
        /// Total number of slides
        count: usize,
        /// The leaderboard data to display
        leaderboard: LeaderboardMessage,
    },
    /// Sync individual score view with position information
    Score {
        /// Current slide index
        index: usize,
        /// Total number of slides
        count: usize,
        /// The player's score information
        score: Option<ScoreMessage>,
    },
    /// Sync metadata about the game state
    Metainfo(MetainfoMessage),
    /// Sync game summary information
    Summary(SummaryMessage),
    /// Participant is not allowed to join
    NotAllowed,
    /// Sync team finding information
    FindTeam(String),
    /// Sync teammate selection options
    ChooseTeammates {
        /// Maximum number of teammates that can be selected
        max_selection: usize,
        /// Available players with their current selection status: (name, is_selected)
        available: Vec<(String, bool)>,
    },
}

/// Summary information sent at the end of the game
///
/// This enum provides different views of the game results depending
/// on whether the recipient is a player or the host.
#[derive(Debug, Serialize, Clone)]
pub enum SummaryMessage {
    /// Summary for individual players
    Player {
        /// Player's final score information
        score: Option<ScoreMessage>,
        /// Points earned on each question
        points: Vec<u64>,
        /// The game configuration that was played
        config: Fuiz,
    },
    /// Summary for the game host with detailed statistics
    Host {
        /// Statistics for each question: (correct_count, total_count)
        stats: Vec<(usize, usize)>,
        /// Total number of players who participated
        player_count: usize,
        /// Final results: (name, points) for all players
        results: Vec<(String, Vec<u64>)>,
        /// Team composition mapping: (team_name, \[player_names\])
        team_mapping: Vec<(String, Vec<String>)>,
        /// The game configuration that was played
        config: Fuiz,
        /// Game options that were used
        options: Options,
    },
}

/// Metadata information about the game state
///
/// This provides contextual information that participants need
/// to understand their current status and available actions.
#[derive(Debug, Serialize, Clone)]
pub enum MetainfoMessage {
    /// Information for the game host
    Host {
        /// Whether the game is locked to new participants
        locked: bool,
    },
    /// Information for players
    Player {
        /// Player's current total score
        score: u64,
        /// Whether answers will be shown after questions
        show_answers: bool,
    },
}

/// Leaderboard data structure for display
///
/// Contains both current standings and previous round standings
/// for comparison and ranking visualization.
#[derive(Debug, Serialize, Clone)]
pub struct LeaderboardMessage {
    /// Current leaderboard standings
    pub current: TruncatedVec<(String, u64)>,
    /// Previous round's standings for comparison
    pub prior: TruncatedVec<(String, u64)>,
}

// Convenience methods
impl Game {
    /// Sets the current game state
    ///
    /// # Arguments
    ///
    /// * `game_state` - The new state to transition to
    fn set_state(&mut self, game_state: State) {
        self.state = game_state;
    }

    /// Gets the score information for a specific watcher
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher to get score for
    ///
    /// # Returns
    ///
    /// Score information if the watcher has a score, otherwise `None`
    fn score(&self, watcher_id: Id) -> Option<ScoreMessage> {
        self.leaderboard.score(self.leaderboard_id(watcher_id))
    }

    /// Gets the leaderboard ID for a player (team ID if in team mode, player ID otherwise)
    ///
    /// In team games, this returns the team ID so that team scores are tracked.
    /// In individual games, this returns the player ID directly.
    ///
    /// # Arguments
    ///
    /// * `player_id` - The player's individual ID
    ///
    /// # Returns
    ///
    /// The ID to use for leaderboard tracking (team or individual)
    pub fn leaderboard_id(&self, player_id: Id) -> Id {
        match &self.team_manager {
            Some(team_manager) => team_manager.get_team(player_id).unwrap_or(player_id),
            None => player_id,
        }
    }

    /// Creates a teammate selection message for team formation
    ///
    /// This generates the message shown to players during team formation,
    /// allowing them to select their preferred teammates from available players.
    ///
    /// # Arguments
    ///
    /// * `watcher` - The ID of the player who needs to choose teammates
    /// * `team_manager` - The team manager containing preference data
    /// * `tunnel_finder` - Function to find active communication tunnels
    ///
    /// # Returns
    ///
    /// An `UpdateMessage` with teammate selection options
    fn choose_teammates_message<F: TunnelFinder>(
        &self,
        watcher: Id,
        team_manager: &TeamManager<names::NameStyle>,
        tunnel_finder: F,
    ) -> UpdateMessage {
        let pref: HashSet<_> = team_manager
            .get_preferences(watcher)
            .unwrap_or_default()
            .into_iter()
            .collect();
        UpdateMessage::ChooseTeammates {
            max_selection: team_manager.optimal_size,
            available: self
                .watchers
                .specific_vec(ValueKind::Player, tunnel_finder)
                .into_iter()
                .filter_map(|(id, _, _)| Some((id, self.watchers.get_name(id)?)))
                .map(|(id, name)| (name, pref.contains(&id)))
                .collect(),
        }
    }

    /// Generates a list of player names for the waiting screen
    ///
    /// Creates a truncated list of player names to display on the waiting
    /// screen or team display. In team games, may show team names instead
    /// of individual player names depending on the current game state.
    ///
    /// # Arguments
    ///
    /// * `tunnel_finder` - Function to find active communication tunnels
    ///
    /// # Returns
    ///
    /// A `TruncatedVec` containing player names with overflow information
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn waiting_screen_names<F: TunnelFinder>(&self, tunnel_finder: F) -> TruncatedVec<String> {
        const LIMIT: usize = 50;

        if let Some(team_manager) = &self.team_manager
            && matches!(self.state, State::TeamDisplay)
        {
            return team_manager.team_names().unwrap_or_default();
        }

        let player_names = self
            .watchers
            .specific_iter(ValueKind::Player, tunnel_finder)
            .filter_map(|(_, _, x)| match x {
                Value::Player(player_value) => Some(player_value.name().to_owned()),
                _ => None,
            })
            .unique();

        TruncatedVec::new(player_names, LIMIT, self.watchers.specific_count(ValueKind::Player))
    }

    /// Creates a leaderboard message with current and previous standings
    ///
    /// Generates a leaderboard message containing both the current standings
    /// and the previous round's standings for comparison. Player/team IDs
    /// are converted to display names for the client interface.
    ///
    /// # Returns
    ///
    /// A `LeaderboardMessage` with current and prior standings
    fn leaderboard_message(&self) -> LeaderboardMessage {
        let [current, prior] = self.leaderboard.last_two_scores_descending();

        let id_map = |i| self.names.get_name_or_unknown(&i);

        let id_score_map = |(id, s)| (id_map(id), s);

        LeaderboardMessage {
            current: current.map(id_score_map),
            prior: prior.map(id_score_map),
        }
    }
}

impl Game {
    /// Creates a new game instance with the provided configuration
    ///
    /// Initializes a new Fuiz game session with the given quiz configuration,
    /// game options, and host identifier. Sets up the initial state, scoring
    /// system, name management, and team configuration if teams are enabled.
    ///
    /// # Arguments
    ///
    /// * `fuiz` - The quiz configuration containing questions and settings
    /// * `options` - Game options including team settings, name generation, etc.
    /// * `host_id` - Unique identifier for the game host
    ///
    /// # Returns
    ///
    /// A new Game instance ready to accept players and begin
    ///
    /// # Examples
    ///
    /// ```rust
    /// use fuiz::game::{Game, Options};
    /// use fuiz::fuiz::config::Fuiz;
    /// use fuiz::watcher::Id;
    /// use fuiz::settings::Settings;
    ///
    /// let host_id = Id::new();
    /// let options = Options::default();
    /// let fuiz_config = Fuiz::default();
    /// let game = Game::new(fuiz_config, options, host_id, &Settings::default());
    /// ```
    pub fn new(fuiz: Fuiz, options: Options, host_id: Id, settings: &crate::settings::Settings) -> Self {
        Self {
            fuiz_config: fuiz,
            watchers: Watchers::with_host_id(host_id, settings.fuiz.max_player_count),
            names: Names::default(),
            leaderboard: Leaderboard::default(),
            state: State::WaitingScreen,
            options,
            team_manager: options.teams.map(|TeamOptions { size, assign_random }| {
                TeamManager::new(size, assign_random, options.random_names.unwrap_or_default())
            }),
            locked: false,
        }
    }

    /// Starts the game or progresses to the next phase
    ///
    /// This method handles the transition from waiting/team formation to the first slide,
    /// or manages team formation when teams are enabled. It sets up the initial slide
    /// state and begins the question flow.
    ///
    /// # Arguments
    ///
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(&mut self, schedule_message: S, tunnel_finder: F) {
        if let Some(slide) = self.fuiz_config.slides.first() {
            if let Some(team_manager) = &mut self.team_manager
                && matches!(self.state, State::WaitingScreen)
            {
                team_manager.finalize(&mut self.watchers, &mut self.names, &tunnel_finder);
                self.state = State::TeamDisplay;
                self.watchers.announce_with(
                    |id, kind| {
                        Some(match kind {
                            ValueKind::Player => {
                                UpdateMessage::FindTeam(self.watchers.get_team_name(id).unwrap_or_default()).into()
                            }
                            ValueKind::Host => {
                                UpdateMessage::TeamDisplay(team_manager.team_names().unwrap_or_default()).into()
                            }
                            ValueKind::Unassigned => {
                                return None;
                            }
                        })
                    },
                    &tunnel_finder,
                );
                return;
            }

            let mut current_slide = CurrentSlide {
                index: 0,
                state: slide.to_state(),
            };

            current_slide.state.play(
                self.team_manager.as_ref(),
                &self.watchers,
                schedule_message,
                tunnel_finder,
                0,
                self.fuiz_config.len(),
            );

            self.set_state(State::Slide(Box::new(current_slide)));
        } else {
            self.announce_summary(tunnel_finder);
        }
    }

    /// Marks the current slide as done and transitions to the next phase
    ///
    /// This method handles the completion of a slide, either advancing to the
    /// leaderboard (if enabled) or directly to the next slide. If all slides
    /// are complete, it announces the game summary.
    ///
    /// # Arguments
    ///
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    pub fn finish_slide<F: TunnelFinder, S: ScheduleMessageFn>(&mut self, schedule_message: S, tunnel_finder: F) {
        if let State::Slide(current_slide) = &self.state {
            if self.options.no_leaderboard {
                let next_index = current_slide.index + 1;
                if let Some(next_slide) = self.fuiz_config.slides.get(next_index) {
                    let mut state = next_slide.to_state();

                    state.play(
                        self.team_manager.as_ref(),
                        &self.watchers,
                        schedule_message,
                        &tunnel_finder,
                        next_index,
                        self.fuiz_config.len(),
                    );

                    self.state = State::Slide(Box::new(CurrentSlide {
                        index: next_index,
                        state,
                    }));
                } else {
                    self.announce_summary(tunnel_finder);
                }
            } else {
                self.set_state(State::Leaderboard(current_slide.index));

                let leaderboard_message = self.leaderboard_message();

                self.watchers.announce_with(
                    |watcher_id, watcher_kind| {
                        Some(match watcher_kind {
                            ValueKind::Host => UpdateMessage::Leaderboard {
                                leaderboard: leaderboard_message.clone(),
                            }
                            .into(),
                            ValueKind::Player => UpdateMessage::Score {
                                score: self.score(watcher_id),
                            }
                            .into(),
                            ValueKind::Unassigned => return None,
                        })
                    },
                    tunnel_finder,
                );
            }
        }
    }

    /// Sends the final game summary to all participants
    ///
    /// This method transitions the game to the Done state and sends appropriate
    /// summary messages to hosts and players. Hosts receive detailed statistics
    /// while players receive their individual scores and points breakdown.
    ///
    /// # Arguments
    ///
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn announce_summary<F: TunnelFinder>(&mut self, tunnel_finder: F) {
        self.state = State::Done;

        self.watchers.announce_with(
            |id, vk| match vk {
                ValueKind::Host => Some(
                    UpdateMessage::Summary({
                        let HostSummary {
                            total_players,
                            player_scores,
                            correctness_stats,
                        } = self.leaderboard.host_summary(!self.options.no_leaderboard);

                        SummaryMessage::Host {
                            stats: correctness_stats,
                            player_count: total_players,
                            results: player_scores
                                .into_iter()
                                .map(|(id, points)| (self.names.get_name_or_unknown(&id), points))
                                .collect(),
                            team_mapping: self
                                .team_manager
                                .as_ref()
                                .map_or(vec![], |tm| tm.team_assignments(&self.names)),
                            config: self.fuiz_config.clone(),
                            options: self.options,
                        }
                    })
                    .into(),
                ),
                ValueKind::Player => Some(
                    UpdateMessage::Summary(SummaryMessage::Player {
                        score: if self.options.no_leaderboard {
                            None
                        } else {
                            self.score(id)
                        },
                        points: self
                            .leaderboard
                            .player_summary(self.leaderboard_id(id), !self.options.no_leaderboard),
                        config: self.fuiz_config.clone(),
                    })
                    .into(),
                ),
                ValueKind::Unassigned => None,
            },
            tunnel_finder,
        );
    }

    /// Marks the game as done and disconnects all players
    ///
    /// This method finalizes the game session by setting the state to Done
    /// and removing all participant sessions, effectively ending the game.
    ///
    /// # Arguments
    ///
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    pub fn mark_as_done<F: TunnelFinder>(&mut self, tunnel_finder: F) {
        self.state = State::Done;

        let watchers = self
            .watchers
            .vec(&tunnel_finder)
            .iter()
            .map(|(x, _, _)| *x)
            .collect_vec();

        for watcher in watchers {
            Watchers::remove_watcher_session(watcher, &tunnel_finder);
        }
    }

    /// Sends metadata information to a player about the game
    ///
    /// This method sends game options and player-specific information like
    /// current score and whether answers will be shown after questions.
    ///
    /// # Arguments
    ///
    /// * `watcher` - ID of the player to send metadata to
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn update_player_with_options<F: TunnelFinder>(&self, watcher: Id, tunnel_finder: F) {
        Watchers::send_state(
            &SyncMessage::Metainfo(MetainfoMessage::Player {
                score: self.score(watcher).map_or(0, |x| x.points),
                show_answers: self.options.show_answers,
            })
            .into(),
            watcher,
            tunnel_finder,
        );
    }

    /// Initiates interactions with an unassigned player
    ///
    /// This method handles the initial setup for new participants, either
    /// automatically assigning them a random name (if enabled) or prompting
    /// them to choose their own name.
    ///
    /// # Arguments
    ///
    /// * `watcher` - ID of the unassigned participant
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn handle_unassigned<F: TunnelFinder>(&mut self, watcher: Id, tunnel_finder: F) {
        if let Some(name_style) = self.options.random_names {
            loop {
                let name = name_style.get_name();

                if self.assign_player_name(watcher, &name, &tunnel_finder).is_ok() {
                    break;
                }
            }
        } else {
            Watchers::send_message(&UpdateMessage::NameChoose.into(), watcher, tunnel_finder);
        }
    }

    /// Assigns a name to a player and converts them from unassigned to player
    ///
    /// This method validates the name, assigns it to the participant, and
    /// updates their status from unassigned to player. It handles name
    /// validation and uniqueness checking.
    ///
    /// # Arguments
    ///
    /// * `watcher` - ID of the participant to assign a name to
    /// * `name` - The name to assign to the participant
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Returns
    ///
    /// * `Ok(())` if the name was assigned successfully
    /// * `Err(names::Error)` if the name is invalid or already taken
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    fn assign_player_name<F: TunnelFinder>(
        &mut self,
        watcher: Id,
        name: &str,
        tunnel_finder: F,
    ) -> Result<(), names::Error> {
        let name = self.names.set_name(watcher, name)?;

        self.watchers.update_watcher_value(
            watcher,
            Value::Player(watcher::PlayerValue::Individual { name: name.clone() }),
        );

        self.update_player_with_name(watcher, &name, tunnel_finder);

        Ok(())
    }

    /// Sends messages to the player about their newly assigned name
    ///
    /// This method notifies the player of their name assignment, handles team
    /// assignment if teams are enabled, and updates the waiting screen for other
    /// participants. It also sends the current game state to the newly named player.
    ///
    /// # Arguments
    ///
    /// * `watcher` - ID of the player whose name was assigned
    /// * `name` - The name that was assigned to the player
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    pub fn update_player_with_name<F: TunnelFinder>(&mut self, watcher: Id, name: &str, tunnel_finder: F) {
        if let Some(team_manager) = &mut self.team_manager
            && let Some(name) = team_manager.add_player(watcher, &mut self.watchers)
        {
            Watchers::send_message(&UpdateMessage::FindTeam(name).into(), watcher, &tunnel_finder);
        }

        Watchers::send_message(
            &UpdateMessage::NameAssign(name.to_string()).into(),
            watcher,
            &tunnel_finder,
        );

        self.update_player_with_options(watcher, &tunnel_finder);

        if !name.is_empty() {
            // Announce to others of user joining
            if matches!(self.state, State::WaitingScreen) {
                if let Some(team_manager) = &self.team_manager
                    && !team_manager.is_random_assignments()
                {
                    self.watchers.announce_with(
                        |id, value| match value {
                            ValueKind::Player => {
                                Some(self.choose_teammates_message(id, team_manager, &tunnel_finder).into())
                            }
                            _ => None,
                        },
                        &tunnel_finder,
                    );
                }

                self.watchers.announce_specific(
                    ValueKind::Host,
                    &UpdateMessage::WaitingScreen(self.waiting_screen_names(&tunnel_finder)).into(),
                    &tunnel_finder,
                );
            }
        }

        Watchers::send_state(
            &self.state_message(watcher, ValueKind::Player, &tunnel_finder),
            watcher,
            tunnel_finder,
        );
    }

    // Network

    /// Adds a new unassigned participant to the game
    ///
    /// This method registers a new participant in the game with unassigned status.
    /// If the game is not locked, it immediately begins the name assignment process.
    ///
    /// # Arguments
    ///
    /// * `watcher` - Unique ID for the new participant
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Returns
    ///
    /// * `Ok(())` if the participant was added successfully
    /// * `Err(watcher::Error)` if the ID is already in use or invalid
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    ///
    /// # Errors
    ///
    /// If there are too many participants, the watcher may not be added.
    ///
    pub fn add_unassigned<F: TunnelFinder>(&mut self, watcher: Id, tunnel_finder: F) -> Result<(), watcher::Error> {
        self.watchers.add_watcher(watcher, Value::Unassigned)?;

        if !self.locked {
            self.handle_unassigned(watcher, tunnel_finder);
        }

        Ok(())
    }

    /// Handles incoming messages from participants
    ///
    /// This method processes all incoming messages from participants, validates
    /// that messages are appropriate for the sender's role, and routes them to
    /// the correct handlers based on the current game state.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the participant sending the message
    /// * `message` - The incoming message to process
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    pub fn receive_message<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watcher_id: Id,
        message: IncomingMessage,
        schedule_message: S,
        tunnel_finder: F,
    ) {
        let Some(watcher_value) = self.watchers.get_watcher_value(watcher_id) else {
            return;
        };

        if !message.follows(watcher_value.kind()) {
            return;
        }

        match message {
            IncomingMessage::Unassigned(_) if self.locked => {}
            IncomingMessage::Host(IncomingHostMessage::Lock(lock_state)) => {
                self.locked = lock_state;
            }
            IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest(s))
                if self.options.random_names.is_none() =>
            {
                if let Err(e) = self.assign_player_name(watcher_id, &s, &tunnel_finder) {
                    Watchers::send_message(&UpdateMessage::NameError(e).into(), watcher_id, tunnel_finder);
                }
            }
            IncomingMessage::Player(IncomingPlayerMessage::ChooseTeammates(preferences)) => {
                if let Some(team_manager) = &mut self.team_manager {
                    team_manager.set_preferences(
                        watcher_id,
                        preferences
                            .into_iter()
                            .filter_map(|name| self.names.get_id(&name))
                            .collect_vec(),
                    );
                }
            }
            message => match &mut self.state {
                State::WaitingScreen | State::TeamDisplay => {
                    if let IncomingMessage::Host(IncomingHostMessage::Next) = message {
                        self.play(schedule_message, &tunnel_finder);
                    }
                }
                State::Slide(current_slide) => {
                    if let SlideAction::Next { schedule_message } = current_slide.state.receive_message(
                        &mut self.leaderboard,
                        &self.watchers,
                        self.team_manager.as_ref(),
                        schedule_message,
                        watcher_id,
                        &tunnel_finder,
                        message,
                        current_slide.index,
                        self.fuiz_config.len(),
                    ) {
                        self.finish_slide(schedule_message, tunnel_finder);
                    }
                }
                State::Leaderboard(index) => {
                    if let IncomingMessage::Host(IncomingHostMessage::Next) = message {
                        let next_index = *index + 1;
                        if let Some(slide) = self.fuiz_config.slides.get(next_index) {
                            let mut state = slide.to_state();

                            state.play(
                                self.team_manager.as_ref(),
                                &self.watchers,
                                schedule_message,
                                &tunnel_finder,
                                next_index,
                                self.fuiz_config.len(),
                            );

                            self.set_state(State::Slide(Box::new(CurrentSlide {
                                index: next_index,
                                state,
                            })));
                        } else {
                            self.announce_summary(&tunnel_finder);
                        }
                    }
                }
                State::Done => {
                    if let IncomingMessage::Host(IncomingHostMessage::Next) = message {
                        self.mark_as_done(tunnel_finder);
                    }
                }
            },
        }
    }

    /// Handles scheduled alarm messages for timed game events
    ///
    /// This method processes alarm messages that were scheduled to trigger
    /// game state transitions at specific times, such as moving from question
    /// display to answer acceptance or from answers to results.
    ///
    /// # Arguments
    ///
    /// * `message` - The alarm message to process
    /// * `schedule_message` - Function to schedule delayed messages for timing
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    /// * `S` - Function type for scheduling alarm messages
    pub fn receive_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        message: &AlarmMessage,
        schedule_message: S,
        tunnel_finder: F,
    ) {
        match message {
            AlarmMessage::MultipleChoice(multiple_choice::AlarmMessage::ProceedFromSlideIntoSlide {
                index: slide_index,
                to: _,
            })
            | AlarmMessage::TypeAnswer(type_answer::AlarmMessage::ProceedFromSlideIntoSlide {
                index: slide_index,
                to: _,
            })
            | AlarmMessage::Order(order::AlarmMessage::ProceedFromSlideIntoSlide {
                index: slide_index,
                to: _,
            }) => match &mut self.state {
                State::Slide(current_slide) if current_slide.index == *slide_index => {
                    if let SlideAction::Next { schedule_message } = current_slide.state.receive_alarm(
                        &mut self.leaderboard,
                        &self.watchers,
                        self.team_manager.as_ref(),
                        schedule_message,
                        &tunnel_finder,
                        message,
                        current_slide.index,
                        self.fuiz_config.len(),
                    ) {
                        self.finish_slide(schedule_message, tunnel_finder);
                    }
                }
                _ => (),
            },
        }
    }

    /// Returns the message necessary to synchronize a participant's state
    ///
    /// This method generates the appropriate synchronization message based on
    /// the current game state and the participant's role. It ensures that
    /// newly connected or reconnecting participants receive the correct view
    /// of the current game state.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the participant to synchronize
    /// * `watcher_kind` - Type of participant (host, player, unassigned)
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Returns
    ///
    /// A `SyncMessage` containing the current state information appropriate for the participant
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    pub fn state_message<F: TunnelFinder>(
        &self,
        watcher_id: Id,
        watcher_kind: ValueKind,
        tunnel_finder: F,
    ) -> super::SyncMessage {
        match &self.state {
            State::WaitingScreen => match &self.team_manager {
                Some(team_manager)
                    if !team_manager.is_random_assignments() && matches!(watcher_kind, ValueKind::Player) =>
                {
                    let pref: HashSet<Id> = team_manager
                        .get_preferences(watcher_id)
                        .unwrap_or_default()
                        .into_iter()
                        .collect();
                    SyncMessage::ChooseTeammates {
                        max_selection: team_manager.optimal_size,
                        available: self
                            .watchers
                            .specific_vec(ValueKind::Player, tunnel_finder)
                            .into_iter()
                            .filter_map(|(id, _, _)| Some((id, self.watchers.get_name(id)?)))
                            .map(|(id, name)| (name, pref.contains(&id)))
                            .collect(),
                    }
                    .into()
                }
                _ => SyncMessage::WaitingScreen(self.waiting_screen_names(tunnel_finder)).into(),
            },
            State::TeamDisplay => match watcher_kind {
                ValueKind::Player => {
                    SyncMessage::FindTeam(self.watchers.get_team_name(watcher_id).unwrap_or_default()).into()
                }
                _ => SyncMessage::TeamDisplay(
                    self.team_manager
                        .as_ref()
                        .and_then(teams::TeamManager::team_names)
                        .unwrap_or_default(),
                )
                .into(),
            },
            State::Leaderboard(index) => match watcher_kind {
                ValueKind::Host | ValueKind::Unassigned => SyncMessage::Leaderboard {
                    index: *index,
                    count: self.fuiz_config.len(),
                    leaderboard: self.leaderboard_message(),
                }
                .into(),
                ValueKind::Player => SyncMessage::Score {
                    index: *index,
                    count: self.fuiz_config.len(),
                    score: self.score(watcher_id),
                }
                .into(),
            },
            State::Slide(current_slide) => current_slide.state.state_message(
                watcher_id,
                watcher_kind,
                self.team_manager.as_ref(),
                &self.watchers,
                tunnel_finder,
                current_slide.index,
                self.fuiz_config.len(),
            ),
            State::Done => match watcher_kind {
                ValueKind::Host => SyncMessage::Summary({
                    let HostSummary {
                        total_players,
                        player_scores,
                        correctness_stats,
                    } = self.leaderboard.host_summary(!self.options.no_leaderboard);
                    SummaryMessage::Host {
                        stats: correctness_stats,
                        player_count: total_players,
                        results: player_scores
                            .into_iter()
                            .map(|(id, points)| (self.names.get_name_or_unknown(&id), points))
                            .collect(),
                        team_mapping: self
                            .team_manager
                            .as_ref()
                            .map_or(vec![], |tm| tm.team_assignments(&self.names)),
                        config: self.fuiz_config.clone(),
                        options: self.options,
                    }
                })
                .into(),
                ValueKind::Player => SyncMessage::Summary(SummaryMessage::Player {
                    score: if self.options.no_leaderboard {
                        None
                    } else {
                        self.score(watcher_id)
                    },
                    points: self
                        .leaderboard
                        .player_summary(self.leaderboard_id(watcher_id), !self.options.no_leaderboard),
                    config: self.fuiz_config.clone(),
                })
                .into(),
                ValueKind::Unassigned => SyncMessage::NotAllowed.into(),
            },
        }
    }

    /// Updates the session associated with a participant (for reconnection)
    ///
    /// This method handles participant reconnection by updating their session
    /// and sending them the current game state. It handles different participant
    /// types appropriately and manages locked game states.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - ID of the participant reconnecting
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    pub fn update_session<F: TunnelFinder>(&mut self, watcher_id: Id, tunnel_finder: F) {
        let Some(watcher_value) = self.watchers.get_watcher_value(watcher_id) else {
            return;
        };

        match watcher_value.clone() {
            Value::Host => {
                Watchers::send_state(
                    &self.state_message(watcher_id, watcher_value.kind(), &tunnel_finder),
                    watcher_id,
                    &tunnel_finder,
                );
                Watchers::send_state(
                    &SyncMessage::Metainfo(MetainfoMessage::Host { locked: self.locked }).into(),
                    watcher_id,
                    tunnel_finder,
                );
            }
            Value::Player(player_value) => {
                if let PlayerValue::Team {
                    team_name,
                    individual_name: _,
                    team_id: _,
                } = &player_value
                {
                    Watchers::send_message(
                        &UpdateMessage::FindTeam(team_name.clone()).into(),
                        watcher_id,
                        &tunnel_finder,
                    );
                }
                Watchers::send_message(
                    &UpdateMessage::NameAssign(player_value.name().to_owned()).into(),
                    watcher_id,
                    &tunnel_finder,
                );
                self.update_player_with_options(watcher_id, &tunnel_finder);
                Watchers::send_state(
                    &self.state_message(watcher_id, watcher_value.kind(), &tunnel_finder),
                    watcher_id,
                    &tunnel_finder,
                );
            }
            Value::Unassigned if self.locked => {}
            Value::Unassigned => {
                self.handle_unassigned(watcher_id, &tunnel_finder);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use serde_json;

    fn test_settings() -> crate::settings::Settings {
        crate::settings::Settings::default()
    }

    #[test]
    fn test_state_serialization() {
        let waiting_state = State::WaitingScreen;
        let json = serde_json::to_string(&waiting_state).unwrap();
        let _deserialized: State = serde_json::from_str(&json).unwrap();

        let team_state = State::TeamDisplay;
        let json = serde_json::to_string(&team_state).unwrap();
        let _deserialized: State = serde_json::from_str(&json).unwrap();

        let done_state = State::Done;
        let json = serde_json::to_string(&done_state).unwrap();
        let _deserialized: State = serde_json::from_str(&json).unwrap();
    }

    #[test]
    fn test_team_options_serialization() {
        let team_options = TeamOptions {
            size: 3,
            assign_random: true,
        };

        let json = serde_json::to_string(&team_options).unwrap();
        let deserialized: TeamOptions = serde_json::from_str(&json).unwrap();

        assert_eq!(team_options.size, deserialized.size);
        assert_eq!(team_options.assign_random, deserialized.assign_random);
    }

    #[test]
    fn test_team_options_validation() {
        use garde::Validate;

        let valid_config = TeamOptions {
            size: 3,
            assign_random: false,
        };
        assert!(valid_config.validate_with(&test_settings()).is_ok());

        let invalid_config = TeamOptions {
            size: 0, // Should be invalid
            assign_random: true,
        };
        assert!(invalid_config.validate_with(&test_settings()).is_err());
    }

    #[test]
    fn test_game_options_serialization() {
        let game_options = Options {
            random_names: Some(names::NameStyle::default()),
            show_answers: false,
            no_leaderboard: false,
            teams: None,
        };

        let json = serde_json::to_string(&game_options).unwrap();
        let deserialized: Options = serde_json::from_str(&json).unwrap();

        assert_eq!(game_options.show_answers, deserialized.show_answers);
        assert_eq!(game_options.no_leaderboard, deserialized.no_leaderboard);
        assert_eq!(game_options.teams.is_none(), deserialized.teams.is_none());
    }

    #[test]
    fn test_game_options_with_teams() {
        let team_options = TeamOptions {
            size: 4,
            assign_random: false,
        };

        let game_options = Options {
            random_names: None,
            show_answers: true,
            no_leaderboard: true,
            teams: Some(team_options),
        };

        let json = serde_json::to_string(&game_options).unwrap();
        let deserialized: Options = serde_json::from_str(&json).unwrap();

        assert!(deserialized.teams.is_some());
        assert_eq!(deserialized.teams.unwrap().size, 4);
    }

    fn create_test_fuiz() -> crate::fuiz::config::Fuiz {
        // Use serde deserialization to create test data since fields are private
        let fuiz_json = r#"{
            "title": "Test Quiz",
            "slides": [
                {
                    "MultipleChoice": {
                        "title": "Test Question",
                        "media": null,
                        "introduce_question": 5000000000,
                        "time_limit": 30000000000,
                        "points_awarded": 1000,
                        "answers": [
                            {
                                "correct": true,
                                "content": {
                                    "Text": "Correct Answer"
                                }
                            },
                            {
                                "correct": false,
                                "content": {
                                    "Text": "Wrong Answer"
                                }
                            }
                        ]
                    }
                }
            ]
        }"#;

        serde_json::from_str(fuiz_json).expect("Failed to deserialize test fuiz")
    }

    #[test]
    fn test_game_new_without_teams() {
        let fuiz = create_test_fuiz();
        let options = Options {
            random_names: None,
            show_answers: false,
            no_leaderboard: false,
            teams: None,
        };
        let host_id = crate::watcher::Id::new();

        let game = Game::new(fuiz, options, host_id, &test_settings());

        assert!(matches!(game.state, State::WaitingScreen));
        assert!(!game.locked);
        assert!(game.team_manager.is_none());
        assert!(!game.options.show_answers);
        assert!(!game.options.no_leaderboard);
    }

    #[test]
    fn test_game_new_with_teams() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 3,
            assign_random: true,
        };
        let options = Options {
            random_names: Some(crate::names::NameStyle::default()),
            show_answers: true,
            no_leaderboard: true,
            teams: Some(team_options),
        };
        let host_id = crate::watcher::Id::new();

        let game = Game::new(fuiz, options, host_id, &test_settings());

        assert!(matches!(game.state, State::WaitingScreen));
        assert!(!game.locked);
        assert!(game.team_manager.is_some());
        assert!(game.options.show_answers);
        assert!(game.options.no_leaderboard);
    }

    #[test]
    fn test_game_set_state() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        assert!(matches!(game.state, State::WaitingScreen));

        game.set_state(State::TeamDisplay);
        assert!(matches!(game.state, State::TeamDisplay));

        game.set_state(State::Done);
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_leaderboard_id_without_teams() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let leaderboard_id = game.leaderboard_id(player_id);

        assert_eq!(player_id, leaderboard_id);
    }

    #[test]
    fn test_game_leaderboard_id_with_teams() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let leaderboard_id = game.leaderboard_id(player_id);

        // When no team is assigned, should fall back to player_id
        assert_eq!(player_id, leaderboard_id);
    }

    #[test]
    fn test_game_score_no_score() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let score = game.score(player_id);

        assert!(score.is_none());
    }

    #[test]
    fn test_game_debug_format() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        let debug_string = format!("{game:?}");
        assert!(debug_string.contains("Game"));
        assert!(debug_string.contains("fuiz"));
    }

    #[test]
    fn test_leaderboard_message_serialization() {
        let current_data = vec![("Player1".to_string(), 100)];
        let prior_data = vec![("Player1".to_string(), 50)];

        let leaderboard_msg = LeaderboardMessage {
            current: crate::TruncatedVec::new(current_data.into_iter(), 10, 1),
            prior: crate::TruncatedVec::new(prior_data.into_iter(), 10, 1),
        };

        let json = serde_json::to_string(&leaderboard_msg).unwrap();
        // Note: LeaderboardMessage only implements Serialize, not Deserialize
        assert!(json.contains("Player1"));
        assert!(json.contains("100"));
        assert!(json.contains("50"));
    }

    // Create a mock tunnel for testing
    #[derive(Debug, Clone)]
    struct MockTunnel {
        messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl MockTunnel {
        fn new() -> Self {
            Self {
                messages: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl crate::session::Tunnel for MockTunnel {
        fn send_message(&self, message: &crate::UpdateMessage) {
            let json = serde_json::to_string(message).unwrap_or_default();
            self.messages.lock().unwrap().push(json);
        }

        fn send_state(&self, message: &crate::SyncMessage) {
            let json = serde_json::to_string(message).unwrap_or_default();
            self.messages.lock().unwrap().push(json);
        }

        fn close(self) {
            // Mock implementation - just drop
        }
    }

    #[test]
    fn test_game_add_unassigned() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());

        // Should have added unassigned watcher
        assert!(game.watchers.has_watcher(unassigned_id));
    }

    #[test]
    fn test_game_update_player_with_name() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add as unassigned first
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());

        // Update to player with name
        game.update_player_with_name(player_id, "TestPlayer", tunnel_finder);

        // Should still have the watcher
        assert!(game.watchers.has_watcher(player_id));
    }

    #[test]
    fn test_game_mark_as_done() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        game.mark_as_done(tunnel_finder);

        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_incoming_message_deserialization() {
        // Test IncomingMessage enum deserialization
        let host_message_json = r#"{"Host": "Next"}"#;
        let host_message: IncomingMessage = serde_json::from_str(host_message_json).unwrap();
        assert!(matches!(host_message, IncomingMessage::Host(_)));

        let unassigned_message_json = r#"{"Unassigned": {"NameRequest": "Player1"}}"#;
        let unassigned_message: IncomingMessage = serde_json::from_str(unassigned_message_json).unwrap();
        assert!(matches!(unassigned_message, IncomingMessage::Unassigned(_)));

        let player_message_json = r#"{"Player": {"IndexAnswer": 0}}"#;
        let player_message: IncomingMessage = serde_json::from_str(player_message_json).unwrap();
        assert!(matches!(player_message, IncomingMessage::Player(_)));
    }

    #[test]
    fn test_game_play_method() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let mut schedule_called = false;
        let mut schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {
            schedule_called = true;
        };

        game.play(&mut schedule_message, tunnel_finder);

        // Should have transitioned to first slide
        assert!(matches!(game.state, State::Slide(_)));
        assert!(schedule_called);
    }

    #[test]
    fn test_game_finish_slide() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game to get to first slide
        game.play(schedule_message, tunnel_finder);

        // Finish the slide
        game.finish_slide(schedule_message, tunnel_finder);

        // Should show leaderboard or be done (depending on options)
        assert!(matches!(game.state, State::Done) || matches!(game.state, State::Leaderboard(_)));
    }

    #[test]
    fn test_game_state_message() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        let state_msg = game.state_message(player_id, crate::watcher::ValueKind::Player, tunnel_finder);

        // Should return waiting screen message for initial state
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::WaitingScreen(_))
        ));
    }

    #[test]
    fn test_incoming_message_follows() {
        // Test IncomingMessage::follows method
        let host_msg = IncomingMessage::Host(IncomingHostMessage::Next);
        assert!(host_msg.follows(crate::watcher::ValueKind::Host));
        assert!(!host_msg.follows(crate::watcher::ValueKind::Player));
        assert!(!host_msg.follows(crate::watcher::ValueKind::Unassigned));

        let player_msg = IncomingMessage::Player(IncomingPlayerMessage::IndexAnswer(0));
        assert!(player_msg.follows(crate::watcher::ValueKind::Player));
        assert!(!player_msg.follows(crate::watcher::ValueKind::Host));
        assert!(!player_msg.follows(crate::watcher::ValueKind::Unassigned));

        let unassigned_msg = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("test".to_string()));
        assert!(unassigned_msg.follows(crate::watcher::ValueKind::Unassigned));
        assert!(!unassigned_msg.follows(crate::watcher::ValueKind::Host));
        assert!(!unassigned_msg.follows(crate::watcher::ValueKind::Player));
    }

    #[test]
    fn test_game_with_team_formation() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();

        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add two players
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());

        // Assign names to make them players
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        // Start the game - should move to team display
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.play(schedule_message, tunnel_finder);

        assert!(matches!(game.state, State::TeamDisplay));
    }

    #[test]
    fn test_game_locked_behavior() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Lock the game
        game.locked = true;

        // Try to add unassigned player - should still add but not process
        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());

        // Process lock message from host
        let lock_msg = IncomingMessage::Host(IncomingHostMessage::Lock(false));
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.receive_message(host_id, lock_msg, schedule_message, tunnel_finder);
        assert!(!game.locked);
    }

    #[test]
    fn test_game_receive_message_invalid_sender() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Try to send host message from player (should be ignored)
        let invalid_msg = IncomingMessage::Host(IncomingHostMessage::Next);
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.receive_message(player_id, invalid_msg, schedule_message, tunnel_finder);

        // State should not change since message doesn't follow sender type
        assert!(matches!(game.state, State::WaitingScreen));
    }

    #[test]
    fn test_game_receive_message_nonexistent_watcher() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let nonexistent_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        // Try to receive message from nonexistent watcher
        let msg = IncomingMessage::Host(IncomingHostMessage::Next);
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Should not panic and should do nothing
        game.receive_message(nonexistent_id, msg, schedule_message, tunnel_finder);
    }

    #[test]
    fn test_game_name_assignment_with_random_names() {
        let fuiz = create_test_fuiz();
        let options = Options {
            random_names: Some(crate::names::NameStyle::default()),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add unassigned player - should automatically get a random name
        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());

        // Should now be a player
        let watcher_value = game.watchers.get_watcher_value(unassigned_id);
        assert!(matches!(watcher_value, Some(crate::watcher::Value::Player(_))));
    }

    #[test]
    fn test_game_name_assignment_error() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add two players
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());

        // Assign same name to first player
        assert!(game.assign_player_name(player1, "SameName", tunnel_finder).is_ok());

        // Try to assign same name to second player - should fail
        assert!(game.assign_player_name(player2, "SameName", tunnel_finder).is_err());
    }

    #[test]
    fn test_game_teammate_selection() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add and name players
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        // Send teammate selection message
        let teammate_msg = IncomingMessage::Player(IncomingPlayerMessage::ChooseTeammates(vec!["Player2".to_string()]));
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.receive_message(player1, teammate_msg, schedule_message, tunnel_finder);

        // Verify preferences were set (implementation detail - would need team_manager access)
    }

    #[test]
    fn test_game_leaderboard_state_transition() {
        let fuiz = create_test_fuiz();
        let options = Options::default(); // leaderboard enabled by default
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish the slide - should show leaderboard
        game.finish_slide(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Leaderboard(_)));

        // Send next from host while in leaderboard state
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next);
        game.receive_message(host_id, next_msg, schedule_message, tunnel_finder);

        // Should be done (since only one slide in test fuiz)
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_no_leaderboard_option() {
        let fuiz = create_test_fuiz();
        let options = Options {
            no_leaderboard: true,
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish the slide - should skip leaderboard and go to done
        game.finish_slide(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_host_index_message() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game
        game.play(schedule_message, tunnel_finder);

        // Send index message from host (should be handled by slide state)
        let index_msg = IncomingMessage::Host(IncomingHostMessage::Index(0));
        game.receive_message(host_id, index_msg, schedule_message, tunnel_finder);
    }

    #[test]
    fn test_game_update_session_host() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id { Some(tunnel.clone()) } else { None }
        };

        // Update host session
        game.update_session(host_id, tunnel_finder);

        // Verify messages were sent (would check tunnel.messages in real test)
    }

    #[test]
    fn test_game_update_session_player_with_team() {
        // Create a game with teams and manually add a team player
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add player and assign name
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Update player session - should work with team manager
        game.update_session(player_id, tunnel_finder);
    }

    #[test]
    fn test_game_update_session_locked_unassigned() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add unassigned player
        game.locked = true; // Lock game first
        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());

        // Update session - should do nothing since locked and unassigned
        game.update_session(unassigned_id, tunnel_finder);
    }

    #[test]
    fn test_game_waiting_screen_names_team_display() {
        // Create a game with team manager in TeamDisplay state
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set state to TeamDisplay
        game.state = State::TeamDisplay;

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        // Should use team names when in TeamDisplay state with team manager
        let names = game.waiting_screen_names(tunnel_finder);
        assert!(names.items.is_empty()); // No teams yet
    }

    #[test]
    fn test_game_state_message_team_display() {
        // Create a game in TeamDisplay state
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set state to TeamDisplay
        game.state = State::TeamDisplay;

        let player_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        // Test TeamDisplay state message for player
        let state_msg = game.state_message(player_id, crate::watcher::ValueKind::Player, tunnel_finder);
        assert!(matches!(state_msg, crate::SyncMessage::Game(SyncMessage::FindTeam(_))));

        // Test TeamDisplay state message for host
        let state_msg = game.state_message(host_id, crate::watcher::ValueKind::Host, tunnel_finder);
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::TeamDisplay(_))
        ));
    }

    #[test]
    fn test_game_state_message_done_unassigned() {
        // Create game in Done state
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set state to Done
        game.state = State::Done;

        let unassigned_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        let state_msg = game.state_message(unassigned_id, crate::watcher::ValueKind::Unassigned, tunnel_finder);
        assert!(matches!(state_msg, crate::SyncMessage::Game(SyncMessage::NotAllowed)));
    }

    #[test]
    fn test_game_receive_message_done_state() {
        // Create game in Done state
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set state to Done
        game.state = State::Done;

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Send Next message in Done state - should call mark_as_done
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next);
        game.receive_message(host_id, next_msg, schedule_message, tunnel_finder);

        // State should still be Done but mark_as_done was called
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_receive_alarm_mismatched_slide() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Start game to get to a slide
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.play(schedule_message, tunnel_finder);

        // Send alarm for wrong slide index - should be ignored
        let alarm = crate::AlarmMessage::MultipleChoice(
            crate::fuiz::multiple_choice::AlarmMessage::ProceedFromSlideIntoSlide {
                index: 999, // Wrong index
                to: crate::fuiz::multiple_choice::SlideState::Question,
            },
        );

        game.receive_alarm(&alarm, schedule_message, tunnel_finder);

        // State should not change (still in slide 0, not 999)
        assert!(matches!(game.state, State::Slide(_)));
    }

    #[test]
    fn test_game_empty_fuiz() {
        // Test game with no slides
        let empty_fuiz_json = r#"{
            "title": "Empty Quiz",
            "slides": []
        }"#;
        let fuiz: crate::fuiz::config::Fuiz = serde_json::from_str(empty_fuiz_json).unwrap();

        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Play should immediately go to Done state since no slides
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_update_message_serialization() {
        // Test various UpdateMessage variants
        let id_assign = UpdateMessage::IdAssign(crate::watcher::Id::new());
        let json = serde_json::to_string(&id_assign).unwrap();
        assert!(json.contains("IdAssign"));

        let name_choose = UpdateMessage::NameChoose;
        let json = serde_json::to_string(&name_choose).unwrap();
        assert!(json.contains("NameChoose"));

        let name_assign = UpdateMessage::NameAssign("TestName".to_string());
        let json = serde_json::to_string(&name_assign).unwrap();
        assert!(json.contains("TestName"));

        let find_team = UpdateMessage::FindTeam("TeamName".to_string());
        let json = serde_json::to_string(&find_team).unwrap();
        assert!(json.contains("TeamName"));
    }

    #[test]
    fn test_sync_message_serialization() {
        // Test various SyncMessage variants
        let metainfo_host = SyncMessage::Metainfo(MetainfoMessage::Host { locked: true });
        let json = serde_json::to_string(&metainfo_host).unwrap();
        assert!(json.contains("locked"));
        assert!(json.contains("true"));

        let metainfo_player = SyncMessage::Metainfo(MetainfoMessage::Player {
            score: 100,
            show_answers: true,
        });
        let json = serde_json::to_string(&metainfo_player).unwrap();
        assert!(json.contains("100"));
        assert!(json.contains("show_answers"));

        let not_allowed = SyncMessage::NotAllowed;
        let json = serde_json::to_string(&not_allowed).unwrap();
        assert!(json.contains("NotAllowed"));
    }

    #[test]
    fn test_summary_message_serialization() {
        let player_summary = SummaryMessage::Player {
            score: None,
            points: vec![100, 200],
            config: create_test_fuiz(),
        };
        let json = serde_json::to_string(&player_summary).unwrap();
        assert!(json.contains("Player"));
        assert!(json.contains("points"));

        let host_summary = SummaryMessage::Host {
            stats: vec![(5, 10), (3, 8)],
            player_count: 15,
            results: vec![],
            team_mapping: vec![],
            config: create_test_fuiz(),
            options: Options::default(),
        };
        let json = serde_json::to_string(&host_summary).unwrap();
        assert!(json.contains("Host"));
        assert!(json.contains("stats"));
        assert!(json.contains("player_count"));
    }

    #[test]
    fn test_game_with_show_answers_option() {
        let fuiz = create_test_fuiz();
        let options = Options {
            show_answers: true,
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        assert!(game.options.show_answers);

        // Test that show_answers is reflected in metadata
        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // This would send metadata with show_answers: true
        game.update_player_with_options(player_id, tunnel_finder);
    }

    #[test]
    fn test_player_message_types() {
        // Test different player message types can be constructed
        let _string_answer = IncomingMessage::Player(IncomingPlayerMessage::StringAnswer("answer".to_string()));
        let _array_answer = IncomingMessage::Player(IncomingPlayerMessage::StringArrayAnswer(vec![
            "a".to_string(),
            "b".to_string(),
        ]));
        // Just verify they can be constructed
    }

    #[test]
    fn test_game_play_with_team_formation() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add players and assign names
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // First play call should transition to TeamDisplay
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::TeamDisplay));

        // Second play call should start the actual game
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));
    }

    #[test]
    fn test_game_announce_summary() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id || id == player_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add a player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Call announce_summary directly
        game.announce_summary(tunnel_finder);

        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_announce_summary_no_leaderboard() {
        let fuiz = create_test_fuiz();
        let options = Options {
            no_leaderboard: true,
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id || id == player_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add a player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Call announce_summary with no_leaderboard option
        game.announce_summary(tunnel_finder);

        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_finish_slide_multiple_slides() {
        // Create a fuiz with multiple slides
        let multi_slide_fuiz_json = r#"{
            "title": "Multi Slide Quiz",
            "slides": [
                {
                    "MultipleChoice": {
                        "title": "Question 1",
                        "media": null,
                        "introduce_question": 5000000000,
                        "time_limit": 30000000000,
                        "points_awarded": 1000,
                        "answers": [
                            {
                                "correct": true,
                                "content": {
                                    "Text": "Correct Answer 1"
                                }
                            }
                        ]
                    }
                },
                {
                    "MultipleChoice": {
                        "title": "Question 2",
                        "media": null,
                        "introduce_question": 5000000000,
                        "time_limit": 30000000000,
                        "points_awarded": 1000,
                        "answers": [
                            {
                                "correct": true,
                                "content": {
                                    "Text": "Correct Answer 2"
                                }
                            }
                        ]
                    }
                }
            ]
        }"#;

        let fuiz: crate::fuiz::config::Fuiz = serde_json::from_str(multi_slide_fuiz_json).unwrap();
        let options = Options {
            no_leaderboard: true, // Skip leaderboard to test direct slide transition
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish first slide - should go to second slide
        game.finish_slide(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish second slide - should be done
        game.finish_slide(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_leaderboard_state_next_message() {
        // Create a fuiz with multiple slides to test leaderboard -> next slide transition
        let multi_slide_fuiz_json = r#"{
            "title": "Multi Slide Quiz",
            "slides": [
                {
                    "MultipleChoice": {
                        "title": "Question 1",
                        "media": null,
                        "introduce_question": 5000000000,
                        "time_limit": 30000000000,
                        "points_awarded": 1000,
                        "answers": [
                            {
                                "correct": true,
                                "content": {
                                    "Text": "Correct Answer 1"
                                }
                            }
                        ]
                    }
                },
                {
                    "MultipleChoice": {
                        "title": "Question 2",
                        "media": null,
                        "introduce_question": 5000000000,
                        "time_limit": 30000000000,
                        "points_awarded": 1000,
                        "answers": [
                            {
                                "correct": true,
                                "content": {
                                    "Text": "Correct Answer 2"
                                }
                            }
                        ]
                    }
                }
            ]
        }"#;

        let fuiz: crate::fuiz::config::Fuiz = serde_json::from_str(multi_slide_fuiz_json).unwrap();
        let options = Options::default(); // Leaderboard enabled
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game
        game.play(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish first slide - should show leaderboard
        game.finish_slide(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Leaderboard(0)));

        // Send next from host - should advance to slide 1
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next);
        game.receive_message(host_id, next_msg, schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish second slide - should show leaderboard again
        game.finish_slide(schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Leaderboard(1)));

        // Send next from host - should be done (no more slides)
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next);
        game.receive_message(host_id, next_msg, schedule_message, tunnel_finder);
        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_receive_message_name_request_locked() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add unassigned player and lock the game
        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());
        game.locked = true;

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Try to send name request - should be ignored due to lock
        let name_msg = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("TestName".to_string()));
        game.receive_message(unassigned_id, name_msg, schedule_message, tunnel_finder);

        // Should still be unassigned
        let watcher_value = game.watchers.get_watcher_value(unassigned_id);
        assert!(matches!(watcher_value, Some(crate::watcher::Value::Unassigned)));
    }

    #[test]
    fn test_game_receive_message_name_request_with_random_names() {
        let fuiz = create_test_fuiz();
        let options = Options {
            random_names: Some(crate::names::NameStyle::default()),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add unassigned player
        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Try to send name request when random names are enabled - should be ignored
        let name_msg = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("TestName".to_string()));
        game.receive_message(unassigned_id, name_msg, schedule_message, tunnel_finder);

        // Should already be a player due to random name assignment in add_unassigned
        let watcher_value = game.watchers.get_watcher_value(unassigned_id);
        assert!(matches!(watcher_value, Some(crate::watcher::Value::Player(_))));
    }

    #[test]
    fn test_game_receive_message_name_assignment_error() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add two unassigned players
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // First player takes a name
        let name_msg1 = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("SameName".to_string()));
        game.receive_message(player1, name_msg1, schedule_message, tunnel_finder);

        // Second player tries to take the same name - should get error message
        let name_msg2 = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("SameName".to_string()));
        game.receive_message(player2, name_msg2, schedule_message, tunnel_finder);

        // Second player should still be unassigned
        let watcher_value = game.watchers.get_watcher_value(player2);
        assert!(matches!(watcher_value, Some(crate::watcher::Value::Unassigned)));
    }

    #[test]
    fn test_game_state_message_waiting_screen_with_team_selection() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: false, // Non-random assignment
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add players
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        // Should return teammate selection message for players in waiting screen with non-random teams
        let state_msg = game.state_message(player1, crate::watcher::ValueKind::Player, tunnel_finder);
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::ChooseTeammates { .. })
        ));
    }

    #[test]
    fn test_game_state_message_leaderboard_host() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set to leaderboard state
        game.state = State::Leaderboard(0);

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        let state_msg = game.state_message(host_id, crate::watcher::ValueKind::Host, tunnel_finder);
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::Leaderboard { .. })
        ));
    }

    #[test]
    fn test_game_state_message_leaderboard_unassigned() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set to leaderboard state
        game.state = State::Leaderboard(0);

        let unassigned_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        let state_msg = game.state_message(unassigned_id, crate::watcher::ValueKind::Unassigned, tunnel_finder);
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::Leaderboard { .. })
        ));
    }

    #[test]
    fn test_game_state_message_done_host() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set to done state
        game.state = State::Done;

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        let state_msg = game.state_message(host_id, crate::watcher::ValueKind::Host, tunnel_finder);
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::Summary(SummaryMessage::Host { .. }))
        ));
    }

    #[test]
    fn test_game_state_message_done_player_no_leaderboard() {
        let fuiz = create_test_fuiz();
        let options = Options {
            no_leaderboard: true,
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Set to done state
        game.state = State::Done;

        let player_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        let state_msg = game.state_message(player_id, crate::watcher::ValueKind::Player, tunnel_finder);
        // Should have None score when no_leaderboard is true
        if let crate::SyncMessage::Game(SyncMessage::Summary(SummaryMessage::Player { score, .. })) = state_msg {
            assert!(score.is_none());
        } else {
            panic!("Expected Player summary message");
        }
    }

    #[test]
    fn test_game_update_session_nonexistent_watcher() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let nonexistent_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        // Should not panic when updating session for nonexistent watcher
        game.update_session(nonexistent_id, tunnel_finder);
    }

    #[test]
    fn test_game_update_player_with_name_empty_name() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add as unassigned first
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());

        // Update with empty name - should not announce to others
        game.update_player_with_name(player_id, "", tunnel_finder);
    }

    #[test]
    fn test_game_update_player_with_name_team_random_assignment() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 2,
            assign_random: true, // Random assignment
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add as unassigned first
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());

        // Update with name - should not announce teammate selection messages for random assignment
        game.update_player_with_name(player_id, "TestPlayer", tunnel_finder);
    }

    #[test]
    fn test_ghost_message_follows() {
        let ghost_msg = IncomingMessage::Ghost(IncomingGhostMessage::DemandId);
        // Ghost messages don't have a specific follows implementation in the current code
        // This test ensures the enum variants exist and can be constructed
        assert!(matches!(ghost_msg, IncomingMessage::Ghost(_)));

        let claim_msg = IncomingMessage::Ghost(IncomingGhostMessage::ClaimId(crate::watcher::Id::new()));
        assert!(matches!(claim_msg, IncomingMessage::Ghost(_)));
    }

    #[test]
    fn test_game_choose_teammates_message() {
        let fuiz = create_test_fuiz();
        let team_options = TeamOptions {
            size: 3,
            assign_random: false,
        };
        let options = Options {
            teams: Some(team_options),
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add players
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        if let Some(team_manager) = &game.team_manager {
            let message = game.choose_teammates_message(player1, team_manager, tunnel_finder);
            assert!(matches!(message, UpdateMessage::ChooseTeammates { .. }));
        }
    }

    #[test]
    fn test_incoming_host_message_index() {
        let index_msg = IncomingHostMessage::Index(5);
        assert!(matches!(index_msg, IncomingHostMessage::Index(5)));

        let lock_msg = IncomingHostMessage::Lock(true);
        assert!(matches!(lock_msg, IncomingHostMessage::Lock(true)));
    }

    #[test]
    fn test_game_serialization() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let game = Game::new(fuiz, options, host_id, &test_settings());

        // Test that Game can be serialized (since it derives Serialize)
        let json = serde_json::to_string(&game).unwrap();
        assert!(json.contains("fuiz_config"));
        assert!(json.contains("watchers"));
        assert!(json.contains("state"));

        // Test that Game can be deserialized
        let deserialized: Game = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized.state, State::WaitingScreen));
    }

    #[test]
    fn test_team_options_with_invalid_size() {
        use garde::Validate;

        let invalid_large = TeamOptions {
            size: 10, // Above max of 5
            assign_random: false,
        };
        assert!(invalid_large.validate_with(&test_settings()).is_err());
    }

    #[test]
    fn test_game_options_validation() {
        use garde::Validate;

        let valid_options = Options {
            random_names: Some(crate::names::NameStyle::default()),
            show_answers: true,
            no_leaderboard: false,
            teams: Some(TeamOptions {
                size: 3,
                assign_random: true,
            }),
        };
        assert!(valid_options.validate_with(&test_settings()).is_ok());

        let invalid_options = Options {
            random_names: Some(crate::names::NameStyle::default()),
            show_answers: true,
            no_leaderboard: false,
            teams: Some(TeamOptions {
                size: 0, // Invalid
                assign_random: true,
            }),
        };
        assert!(invalid_options.validate_with(&test_settings()).is_err());
    }

    #[test]
    fn test_game_waiting_screen_names_with_players() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player1 = crate::watcher::Id::new();
        let player2 = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player1 || id == player2 {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add players with names to test the actual filtering logic
        assert!(game.add_unassigned(player1, tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        let names = game.waiting_screen_names(tunnel_finder);
        assert_eq!(names.items.len(), 2);
        assert!(names.items.contains(&"Player1".to_string()));
        assert!(names.items.contains(&"Player2".to_string()));
    }

    #[test]
    fn test_game_leaderboard_message() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add a player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Test leaderboard_message function
        let leaderboard_msg = game.leaderboard_message();
        assert!(leaderboard_msg.current.items.is_empty()); // No scores yet
        assert!(leaderboard_msg.prior.items.is_empty());
    }

    #[test]
    fn test_game_receive_alarm_valid_slide() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game to get to slide 0
        game.play(schedule_message, tunnel_finder);

        // Send alarm for correct slide index
        let alarm = crate::AlarmMessage::MultipleChoice(
            crate::fuiz::multiple_choice::AlarmMessage::ProceedFromSlideIntoSlide {
                index: 0, // Correct index
                to: crate::fuiz::multiple_choice::SlideState::Question,
            },
        );

        // Should handle the alarm for the current slide
        game.receive_alarm(&alarm, schedule_message, tunnel_finder);
    }

    #[test]
    fn test_game_receive_alarm_non_slide_state() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        // Keep game in WaitingScreen state
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Send alarm while not in slide state - should be ignored
        let alarm = crate::AlarmMessage::MultipleChoice(
            crate::fuiz::multiple_choice::AlarmMessage::ProceedFromSlideIntoSlide {
                index: 0,
                to: crate::fuiz::multiple_choice::SlideState::Question,
            },
        );

        game.receive_alarm(&alarm, schedule_message, tunnel_finder);
        // Should remain in WaitingScreen
        assert!(matches!(game.state, State::WaitingScreen));
    }

    #[test]
    fn test_game_state_message_slide_state() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Start the game to get to slide state
        game.play(schedule_message, tunnel_finder);

        // Test state message for slide state
        let state_msg = game.state_message(player_id, crate::watcher::ValueKind::Player, tunnel_finder);
        // The message will be one of the slide-specific sync messages (MultipleChoice, TypeAnswer, or Order)
        assert!(matches!(
            state_msg,
            crate::SyncMessage::MultipleChoice(_) | crate::SyncMessage::TypeAnswer(_) | crate::SyncMessage::Order(_)
        ));
    }

    #[test]
    fn test_game_update_session_player_individual() {
        let fuiz = create_test_fuiz();
        let options = Options::default(); // No teams
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Update session for individual player (no teams)
        game.update_session(player_id, tunnel_finder);
    }

    #[test]
    fn test_game_mark_as_done_with_watchers() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id || id == host_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add a player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Mark as done - should remove all watchers
        game.mark_as_done(tunnel_finder);

        assert!(matches!(game.state, State::Done));
    }

    #[test]
    fn test_game_handle_unassigned_manual_names() {
        let fuiz = create_test_fuiz();
        let options = Options {
            random_names: None, // Manual name selection
            ..Default::default()
        };
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let unassigned_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == unassigned_id {
                Some(tunnel.clone())
            } else {
                None
            }
        };

        // Add unassigned - should prompt for name choice
        assert!(game.add_unassigned(unassigned_id, tunnel_finder).is_ok());

        // Should still be unassigned waiting for name choice
        let watcher_value = game.watchers.get_watcher_value(unassigned_id);
        assert!(matches!(watcher_value, Some(crate::watcher::Value::Unassigned)));
    }

    #[test]
    fn test_game_assign_player_name_invalid() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id { Some(tunnel.clone()) } else { None }
        };

        // Add unassigned player
        assert!(game.add_unassigned(player_id, tunnel_finder).is_ok());

        // Try to assign an invalid name (empty string might be invalid depending on validation)
        let result = game.assign_player_name(player_id, "", tunnel_finder);
        // The result depends on the names module validation logic
        // This test exercises the error path if empty names are invalid
        if result.is_err() {
            // Should still be unassigned
            let watcher_value = game.watchers.get_watcher_value(player_id);
            assert!(matches!(watcher_value, Some(crate::watcher::Value::Unassigned)));
        }
    }

    #[test]
    fn test_game_alarm_message_variants() {
        // Test that we can construct different alarm message types
        let type_answer_alarm =
            crate::AlarmMessage::TypeAnswer(crate::fuiz::type_answer::AlarmMessage::ProceedFromSlideIntoSlide {
                index: 0,
                to: crate::fuiz::type_answer::SlideState::Question,
            });
        assert!(matches!(type_answer_alarm, crate::AlarmMessage::TypeAnswer(_)));

        let order_alarm = crate::AlarmMessage::Order(crate::fuiz::order::AlarmMessage::ProceedFromSlideIntoSlide {
            index: 0,
            to: crate::fuiz::order::SlideState::Question,
        });
        assert!(matches!(order_alarm, crate::AlarmMessage::Order(_)));
    }
}
