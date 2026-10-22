//! Core game logic and state management
//!
//! This module contains the main game struct and logic for managing
//! a Fuiz game session, including player management, question flow,
//! scoring, team formation, and real-time communication with all
//! connected participants.

use std::fmt::Debug;
use std::time::Duration;

use garde::Validate;
use itertools::Itertools;
use serde::{Deserialize, Serialize};
use serde_with::DurationMilliSeconds;

use crate::tick::Tick;
use crate::{
    fuiz::{
        brainstorm,
        config::{CurrentSlide, ScheduleMessageFn, SlideAction, SlideState},
        free_text, info_slide, order, pin, poll, scale, slider, type_answer,
    },
    watcher::Value,
};

use super::{
    AlarmMessage,
    fuiz::{config::Fuiz, multiple_choice},
    leaderboard::{HostSummary, Leaderboard, ScoreMessage},
    names::{self, Names, NamingScheme},
    recap::{RecapEntry, SlideRecap},
    session::TunnelFinder,
    teams::{self, TeamManager},
    watcher::{self, Id, PlayerValue, ValueKind, Watchers},
};

/// Represents the current phase or state of the game
///
/// The game progresses through different states, from waiting for players
/// to join, through individual questions, to showing results and completion.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
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

/// Policy for handling potentially profane player/team names.
///
/// Controls whether `Names::set_name` rejects entries that the rustrict
/// content filter flags as inappropriate. The censor adds significant
/// per-name CPU cost (per-character automaton + match table), so trusted
/// environments and benchmarks can opt out.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Profanity {
    /// Reject names that rustrict considers inappropriate.
    #[default]
    Censor,
    /// Skip the rustrict check and accept any non-empty name.
    Allow,
}

/// Whether the game advances past host-paced screens on its own, and how long
/// each stays up first. A `None` delay leaves that screen waiting for the
/// host's "Next".
///
/// Only screens that would otherwise sit until the host acts are covered: a
/// slide's own timers already move its earlier phases along. The lobby and the
/// final summary are never advanced automatically.
///
/// Every field is optional on the wire; a missing one takes its value from
/// [`AutoAdvance::default`], so a client can switch auto-advance on without
/// choosing delays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, Validate)]
#[garde(context(crate::settings::Settings as ctx))]
#[serde(default)]
pub struct AutoAdvance {
    /// Whether auto-advance is on when the game starts. The host can switch
    /// it either way during the game.
    #[garde(skip)]
    pub enabled: bool,
    /// The team display shown before the first slide of a team game.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_auto_advance(val)))]
    #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    pub team_display: Option<Duration>,
    /// A question's results, its last phase.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_auto_advance(val)))]
    #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    pub results: Option<Duration>,
    /// The leaderboard between slides.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_auto_advance(val)))]
    #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    pub leaderboard: Option<Duration>,
    /// An info slide with no duration of its own.
    #[garde(custom(|val, ctx: &crate::settings::Settings| ctx.question.validate_auto_advance(val)))]
    #[serde(with = "serde_with::As::<Option<DurationMilliSeconds<u64>>>")]
    pub info_slide: Option<Duration>,
}

impl Default for AutoAdvance {
    /// Off, with delays long enough to read each screen. The team display is
    /// left to the host: players need time to find their teammates.
    fn default() -> Self {
        Self {
            enabled: false,
            team_display: None,
            results: Some(Duration::from_secs(8)),
            leaderboard: Some(Duration::from_secs(5)),
            info_slide: Some(Duration::from_secs(10)),
        }
    }
}

/// An auto-advance counting down on the host's screen.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
struct PendingAutoAdvance {
    /// The screen it will advance past.
    screen: HostScreen,
    /// When the countdown started.
    since: crate::time::Timestamp,
    /// How long the countdown runs.
    delay: Duration,
}

/// Auto-advance as the host's controls show it.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct AutoAdvanceStatus {
    /// Whether auto-advance is on.
    running: bool,
    /// The advance counting down on the current screen, if any.
    next: Option<NextAdvance>,
}

/// A countdown to an automatic "Next", for [`AutoAdvanceStatus`].
#[derive(Debug, Clone, Copy, Serialize)]
pub struct NextAdvance {
    /// The screen being advanced past. The host compares it against its own
    /// screen, so a countdown never shows on a screen it doesn't belong to.
    screen: HostScreen,
    /// Time left before the game advances.
    #[serde(with = "serde_with::As::<DurationMilliSeconds<u64>>")]
    remaining: Duration,
    /// The full length of the countdown.
    #[serde(with = "serde_with::As::<DurationMilliSeconds<u64>>")]
    delay: Duration,
}

/// Alarm that advances a host-paced screen once its [`AutoAdvance`] delay
/// runs out: a "Next" the game issues to itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoAdvanceAlarm {
    /// The screen the alarm was scheduled from. Like a host's "Next", it is
    /// ignored once the game has moved past that screen.
    screen: HostScreen,
    /// [`Game::auto_advance_epoch`] when the alarm was scheduled. A pause and
    /// resume on the same screen schedules a fresh alarm, and this tells the
    /// older one apart.
    epoch: u64,
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
    /// Profanity-filter policy for submitted player and team names.
    /// Defaults to [`Profanity::Censor`]; existing payloads that omit the
    /// field deserialize to the default.
    #[garde(skip)]
    #[serde(default)]
    profanity: Profanity,
    /// Whether host-paced screens advance on their own, and after how long.
    /// Off by default.
    #[garde(dive)]
    #[serde(default)]
    auto_advance: AutoAdvance,
}

impl Options {
    /// Builder-style override for the profanity policy. Useful for tests
    /// and benchmarks that want to construct an `Options` with the censor off.
    #[must_use]
    pub fn with_profanity(mut self, profanity: Profanity) -> Self {
        self.profanity = profanity;
        self
    }

    /// Builder-style override for the auto-advance settings.
    #[must_use]
    pub fn with_auto_advance(mut self, auto_advance: AutoAdvance) -> Self {
        self.auto_advance = auto_advance;
        self
    }
}

/// The main game session struct
///
/// This struct represents a complete Fuiz game session, managing all
/// aspects of the game including participant connections, question flow,
/// scoring, team management, and real-time communication.
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
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
    /// Whether auto-advance is on. Starts from the options; the host switches
    /// it from then on, and it holds until they switch it again.
    #[cfg_attr(feature = "serializable", serde(default))]
    auto_advance_running: bool,
    /// Bumped every time the host switches auto-advance, so an
    /// [`AutoAdvanceAlarm`] scheduled before the latest switch is recognizable
    /// as stale.
    #[cfg_attr(feature = "serializable", serde(default))]
    auto_advance_epoch: u64,
    /// The most recently scheduled auto-advance, for the host's countdown.
    #[cfg_attr(feature = "serializable", serde(default))]
    auto_advance_pending: Option<PendingAutoAdvance>,
    /// Every finished slide that asked something, in the order played, for
    /// the players' recaps.
    #[cfg_attr(feature = "serializable", serde(default))]
    recap: Vec<SlideRecap>,
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
#[derive(Debug, Deserialize, Serialize, Clone)]
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
#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum IncomingPlayerMessage {
    /// Answer selected by index (for single-answer multiple choice questions)
    IndexAnswer(usize),
    /// Multiple answers selected by indices (for multi-answer multiple choice questions)
    IndexArrayAnswer(Vec<usize>),
    /// Text answer submitted (for type answer questions)
    StringAnswer(String),
    /// Array of strings submitted (for order, word cloud and brainstorm questions)
    StringArrayAnswer(Vec<String>),
    /// A numeric value submitted (for slider and scale questions)
    NumberAnswer(f64),
    /// A point on the slide's image, normalised to `0.0..=1.0` on both axes
    /// (for pin answer and drop pin questions)
    PointAnswer {
        /// Horizontal position as a fraction of the image width.
        x: f64,
        /// Vertical position as a fraction of the image height.
        y: f64,
    },
    /// Substring search for a teammate. Sent while the player is on the
    /// preference-mode picker; the server resolves matches and responds with
    /// either [`UpdateMessage::TeammateSelected`] or
    /// [`UpdateMessage::TeammateSuggestions`].
    SearchTeammate(String),
    /// Remove an exact name from the player's selected-teammates list.
    DeselectTeammate(String),
}

/// Messages that can be sent by unassigned connections
#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum IncomingUnassignedMessage {
    /// Request to set a specific name and become a player
    NameRequest(String),
}

/// Messages that can be sent by disconnected clients trying to reconnect
#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum IncomingGhostMessage {
    /// Request a new ID assignment
    DemandId,
    /// Attempt to reclaim a specific existing ID
    ClaimId(Id),
}

/// The host-facing screen a "Next" command was issued from.
///
/// Echoed back by the client so the server can ignore a stale "Next", e.g.
/// an impatient double-click made before the new screen rendered, which would
/// otherwise advance the slide twice (the answering window getting skipped).
/// A "Next" is acted on only when its screen still matches the live game state
/// (see [`Game::current_host_screen`]).
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum HostScreen {
    /// Pre-game lobby (waiting screen or team display).
    Lobby,
    /// A question slide, tagged with the slide's own phase. Each slide type
    /// carries its own [`Phase`](crate::fuiz::common::Phase) so question types
    /// are free to define their phases independently.
    Slide(SlidePosition),
    /// The post-question leaderboard for the slide at `index`.
    Leaderboard {
        /// Index of the slide whose leaderboard is shown.
        index: usize,
    },
    /// The final summary screen.
    Summary,
}

/// A slide's host-facing position: its index plus its own phase. One variant
/// per question type, mirroring [`AlarmMessage`] so each type keeps its own
/// `Phase` rather than collapsing into a shared enum.
#[derive(Debug, Deserialize, Serialize, Clone, Copy, PartialEq, Eq)]
pub enum SlidePosition {
    /// A multiple-choice slide at `index` showing `phase`.
    MultipleChoice {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: multiple_choice::Phase,
    },
    /// A type-answer slide at `index` showing `phase`.
    TypeAnswer {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: type_answer::Phase,
    },
    /// An order slide at `index` showing `phase`.
    Order {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: order::Phase,
    },
    /// A slider slide at `index` showing `phase`.
    Slider {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: slider::Phase,
    },
    /// A scale slide at `index` showing `phase`.
    Scale {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: scale::Phase,
    },
    /// A poll slide at `index` showing `phase`.
    Poll {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: poll::Phase,
    },
    /// A pin slide at `index` showing `phase`.
    Pin {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: pin::Phase,
    },
    /// A free-text slide at `index` showing `phase`.
    FreeText {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: free_text::Phase,
    },
    /// A brainstorm slide at `index` showing `phase`.
    Brainstorm {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: brainstorm::Phase,
    },
    /// An info slide at `index` showing `phase`.
    InfoSlide {
        /// Index of the slide being shown.
        index: usize,
        /// The slide's current phase.
        phase: info_slide::Phase,
    },
}

/// Messages that can be sent by the game host
#[derive(Debug, Deserialize, Serialize, Clone)]
pub enum IncomingHostMessage {
    /// Advance the screen identified by `HostScreen` to the next phase/slide.
    /// Carries the host's current screen so a stale duplicate is ignored.
    Next(HostScreen),
    /// Jump to a specific slide by index
    Index(usize),
    /// Lock or unlock the game to new participants
    Lock(bool),
    /// Switch auto-advance on (`true`) or off (`false`). Holds until switched
    /// again; the host's own "Next" keeps working either way.
    AutoAdvance(bool),
    /// Remove a player by their assigned name. Their watcher is dropped, so a
    /// reconnect comes back as a fresh unnamed participant rather than the
    /// kicked one.
    Kick(String),
    /// Ask for who answered the current slide and what they said.
    ///
    /// Pulled rather than pushed, so the traffic stays on the host's own socket
    /// and off every player's. The host asks once per results screen, because
    /// the join between names and answers is only available while the slide is
    /// still up, and the end-of-game response log needs every slide's answers.
    RequestResponses,
    /// Ask who is on each team.
    ///
    /// Pulled for the same reason as [`Self::RequestResponses`], and pulled
    /// repeatedly rather than sent once: teams keep changing after they form,
    /// as late joiners are dealt onto them and kicked players drop off.
    RequestTeamRosters,
}

/// One player's answer to the current slide, for the host's response list.
#[derive(Debug, Serialize, Clone)]
pub struct PlayerResponse<'a> {
    /// The player's name.
    pub name: &'a str,
    /// What they answered, phrased by the slide that asked.
    pub answer: String,
}

/// One team's membership, for the host's roster view.
#[derive(Debug, Serialize, Clone)]
pub struct TeamRoster<'a> {
    /// The team's name.
    pub name: &'a str,
    /// Its members, in the order they take turns.
    pub members: Vec<TeamMember<'a>>,
}

/// One member of a team, for [`TeamRoster`].
#[derive(Debug, Serialize, Clone)]
pub struct TeamMember<'a> {
    /// The player's name.
    pub name: &'a str,
    /// Whether they currently hold a socket. A member who drops stays on the
    /// roster, so the host sees a team playing short rather than a team that
    /// silently shrank.
    pub connected: bool,
}

/// Outcome of a reconnection attempt via [`Game::update_session`].
///
/// Returned rather than swallowed so callers cannot silently no-op when the
/// watcher is gone (e.g. after a kick); they must decide what to do with a
/// [`Reconnect::NotFound`].
#[must_use]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reconnect {
    /// The watcher still existed and was re-synced to the current state.
    Reclaimed,
    /// No watcher held this id; the caller should admit a fresh participant.
    NotFound,
}

/// Update messages sent to participants about game state changes
///
/// These messages inform participants about changes that affect their
/// view or interaction with the game.
#[derive(Debug, Serialize, Clone)]
pub enum UpdateMessage<'a> {
    /// Assign a unique ID to a participant
    IdAssign(Id),
    /// A player joined the waiting screen; host-only incremental event.
    /// Carries just the one name, not a full `WaitingScreen` roster.
    PlayerJoined(&'a str),
    /// A player left the waiting screen; host-only incremental event.
    PlayerLeft(&'a str),
    /// Sent to a participant just before they are forcibly removed by the host
    Kicked,
    /// Sent just before the connection is closed because the participant could
    /// not join: the game is locked or at maximum players. The client should
    /// surface the reason and stop reconnecting.
    CannotJoin(watcher::Error),
    /// Update the team display screen with every team name
    TeamDisplay(Vec<&'a str>),
    /// (HOST ONLY): who answered the current slide, and what they answered.
    /// Sent in reply to [`IncomingHostMessage::RequestResponses`].
    ///
    /// Every answer on file, never a sample: this backs the end-of-game response
    /// export, where a dropped row is a missing answer rather than a longer
    /// screen. One entry per player bounds it.
    PlayerResponses(Vec<PlayerResponse<'a>>),
    /// (HOST ONLY): auto-advance was switched, or a countdown started.
    AutoAdvance(AutoAdvanceStatus),
    /// (HOST ONLY): who is on each team, and which of them are still connected.
    /// Sent in reply to [`IncomingHostMessage::RequestTeamRosters`].
    TeamRosters(Vec<TeamRoster<'a>>),
    /// Prompt the participant to choose a name
    NameChoose,
    /// Confirm a name assignment
    NameAssign(&'a str),
    /// Report an error with name validation
    NameError(names::Error),
    /// Send leaderboard information
    Leaderboard {
        /// The leaderboard data to display
        leaderboard: LeaderboardMessage<'a>,
    },
    /// Send individual score information
    Score {
        /// The player's score information
        score: Option<ScoreMessage>,
    },
    /// Send game summary information
    Summary(SummaryMessage<'a>),
    /// Inform player to find a team (team games only)
    FindTeam(&'a str),
    /// Confirm a teammate was added to the player's selection (response to
    /// [`IncomingPlayerMessage::SearchTeammate`] when exactly one match found).
    TeammateSelected {
        /// The matched player's full name.
        name: &'a str,
    },
    /// Disambiguation list of up to five names matching the query (response to
    /// [`IncomingPlayerMessage::SearchTeammate`] when 0 or 2+ matches).
    TeammateSuggestions {
        /// Up to five candidate names, alphabetically sorted.
        suggestions: Vec<&'a str>,
    },
    /// Confirm a teammate was removed (response to
    /// [`IncomingPlayerMessage::DeselectTeammate`]).
    TeammateDeselected {
        /// The removed name.
        name: &'a str,
    },
    /// (PLAYER ONLY): the slide that just finished, for the player's recap.
    RecapEntry(RecapEntry<'a>),
}

/// Sync messages sent to participants to synchronize their view with game state
///
/// These messages are sent when participants connect or when their view
/// needs to be completely synchronized with the current game state.
#[derive(Debug, Serialize, Clone)]
pub enum SyncMessage<'a> {
    /// (HOST ONLY) Sync the lobby with every player's name, in join order.
    /// Incremental updates flow via [`UpdateMessage::PlayerJoined`] /
    /// [`UpdateMessage::PlayerLeft`].
    WaitingScreen(Vec<&'a str>),
    /// Sync the lobby for everyone else, who are shown how many have joined but
    /// not who: names reach the host alone, the same as `PlayerJoined` does.
    WaitingCount(usize),
    /// Sync team display screen with every team name
    TeamDisplay(Vec<&'a str>),
    /// Sync leaderboard view with position information
    Leaderboard {
        /// Current slide index
        index: usize,
        /// Total number of slides
        count: usize,
        /// The leaderboard data to display
        leaderboard: LeaderboardMessage<'a>,
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
    Summary(SummaryMessage<'a>),
    /// Participant is not allowed to join
    NotAllowed,
    /// Sync team finding information
    FindTeam(&'a str),
    /// Sync the preference-mode teammate picker.
    TeammatePicker {
        /// Maximum number of teammates this player may select.
        max_selection: usize,
        /// Names this player has already selected.
        selected: Vec<&'a str>,
    },
    /// (PLAYER ONLY): every finished slide, for the player's recap.
    Recap(Vec<RecapEntry<'a>>),
}

/// Summary information sent at the end of the game
///
/// This enum provides different views of the game results depending
/// on whether the recipient is a player or the host.
///
/// `config` is borrowed from the live `Game` so that broadcasting the summary
/// to every recipient doesn't deep-clone the entire slide list per send.
#[derive(Debug, Serialize, Clone)]
pub enum SummaryMessage<'a> {
    /// Summary for individual players
    Player {
        /// Player's final score information
        score: Option<ScoreMessage>,
        /// Points earned on each question
        points: Vec<u64>,
        /// The game configuration that was played
        config: &'a Fuiz,
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
        config: &'a Fuiz,
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
        /// Whether this is a team game. Sent on every host sync because
        /// `TeamDisplay` only reaches the host while that screen is up, which
        /// leaves a host who reloaded mid-game with no other way to tell.
        teams: bool,
        /// Whether auto-advance is on, and the countdown on this screen.
        auto_advance: AutoAdvanceStatus,
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
pub struct LeaderboardMessage<'a> {
    /// Current leaderboard standings, whole and in descending score order
    pub current: Vec<(&'a str, u64)>,
    /// Previous round's standings for comparison
    pub prior: Vec<(&'a str, u64)>,
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

    /// One finished slide from `player`'s side.
    ///
    /// In a team game the team scores on its earliest answer, so every member
    /// is shown that one, whoever gave it, and the verdict agrees with the
    /// points.
    fn recap_entry<'a>(&self, recap: &'a SlideRecap, player: Id, points: Option<u64>) -> RecapEntry<'a> {
        let answerer = self
            .team_manager
            .as_ref()
            .and_then(|team_manager| team_manager.team_members(player))
            .and_then(|members| {
                members
                    .into_iter()
                    .filter_map(|member| recap.answers.get(&member).map(|given| (given.answered_at, member)))
                    .min()
            })
            .map_or(player, |(_, member)| member);
        recap.entry_for(answerer, points)
    }

    /// Every finished slide from `player`'s side, for [`SyncMessage::Recap`].
    fn recap_entries(&self, player: Id) -> Vec<RecapEntry<'_>> {
        let leaderboard_id = self.leaderboard_id(player);
        self.recap
            .iter()
            .map(|recap| {
                let points = (!self.options.no_leaderboard).then(|| {
                    self.leaderboard
                        .round_points(recap.index)
                        .iter()
                        .find(|(id, _)| *id == leaderboard_id)
                        .map_or(0, |(_, points)| *points)
                });
                self.recap_entry(recap, player, points)
            })
            .collect()
    }

    /// Sends every player their side of the slide that just finished.
    fn announce_recap_entry<F: TunnelFinder>(&self, recap: &SlideRecap, tunnel_finder: F) {
        let points: rustc_hash::FxHashMap<Id, u64> = if self.options.no_leaderboard {
            rustc_hash::FxHashMap::default()
        } else {
            self.leaderboard.round_points(recap.index).iter().copied().collect()
        };

        self.watchers.announce_with(
            |id, kind| match kind {
                ValueKind::Player => {
                    let points = (!self.options.no_leaderboard)
                        .then(|| points.get(&self.leaderboard_id(id)).copied().unwrap_or(0));
                    Some(UpdateMessage::RecapEntry(self.recap_entry(recap, id, points)).into())
                }
                ValueKind::Host | ValueKind::Unassigned => None,
            },
            tunnel_finder,
        );
    }

    /// Every player's name for [`SyncMessage::WaitingScreen`], in join order.
    ///
    /// Only called on the rare state sync; per-join updates flow as
    /// `PlayerJoined` / `PlayerLeft` events.
    fn waiting_screen_names<F: TunnelFinder>(&self, tunnel_finder: F) -> Vec<&str> {
        self.watchers
            .specific_iter(ValueKind::Player, tunnel_finder)
            .filter_map(|(id, _, _)| self.names.get_name(&id))
            .collect()
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
    fn leaderboard_message(&self) -> LeaderboardMessage<'_> {
        let [current, prior] = self.leaderboard.last_two_scores_descending();

        let id_map = |i| self.names.get_name_or_unknown(&i);

        let id_score_map = |(id, s)| (id_map(id), s);

        LeaderboardMessage {
            current: current.into_iter().map(id_score_map).collect(),
            prior: prior.into_iter().map(id_score_map).collect(),
        }
    }
}

impl Game {
    /// Answer the host's request for who said what on the current slide.
    ///
    /// Names come from the game's table and the answers from the slide, so this
    /// is the only place the two can be joined. Sorted by name because a host
    /// scanning for one player shouldn't have to hunt.
    ///
    /// Sent whole. One row per player, however many entries that player
    /// submitted, so the game's player ceiling already bounds the payload, and
    /// it goes to the host's own socket rather than to the room.
    fn send_player_responses<F: TunnelFinder>(&self, host_id: Id, tunnel_finder: F) {
        let State::Slide(current_slide) = &self.state else {
            return;
        };

        let mut responses = current_slide
            .state
            .player_answers()
            .into_iter()
            .filter_map(|(id, answer)| self.names.get_name(&id).map(|name| PlayerResponse { name, answer }))
            .collect_vec();
        responses.sort_unstable_by(|a, b| a.name.cmp(b.name));

        Watchers::send_message(
            &UpdateMessage::PlayerResponses(responses).into(),
            host_id,
            tunnel_finder,
        );
    }

    /// Answers [`IncomingHostMessage::RequestTeamRosters`] with every team and
    /// who is on it. Empty for a game that is not in team mode, or whose teams
    /// have not formed yet — an empty list is something the host's screen can
    /// render, where silence is indistinguishable from a lost message.
    ///
    /// Takes `&self`: this is a query and changes nothing, which is what lets
    /// the socket layer answer it without writing an entry to the log.
    /// Callers reaching this directly skip the kind check in
    /// [`Self::receive_message`], so it is repeated here.
    pub fn send_team_rosters<F: TunnelFinder>(&self, host_id: Id, tunnel_finder: F) {
        if !matches!(
            self.watchers.get_watcher_value_ref(host_id).map(Value::kind),
            Some(ValueKind::Host)
        ) {
            return;
        }

        let rosters = self
            .team_manager
            .as_ref()
            .and_then(teams::TeamManager::rosters)
            .map(|teams| {
                teams
                    .map(|(name, members)| TeamRoster {
                        name,
                        members: members
                            .iter()
                            .map(|&id| TeamMember {
                                name: self.names.get_name_or_unknown(&id),
                                connected: Watchers::is_alive(id, &tunnel_finder),
                            })
                            .collect(),
                    })
                    .collect_vec()
            })
            .unwrap_or_default();

        Watchers::send_message(&UpdateMessage::TeamRosters(rosters).into(), host_id, tunnel_finder);
    }

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
        let needs_prefix_index = options.teams.is_some_and(|t| !t.assign_random);
        Self {
            fuiz_config: fuiz,
            watchers: Watchers::with_host_id(host_id, settings.fuiz.max_player_count),
            names: if needs_prefix_index {
                Names::with_prefix_index()
            } else {
                Names::default()
            },
            leaderboard: Leaderboard::default(),
            state: State::WaitingScreen,
            options,
            team_manager: options.teams.map(|TeamOptions { size, assign_random }| {
                TeamManager::new(size, assign_random, options.random_names.unwrap_or_default())
            }),
            locked: false,
            auto_advance_running: options.auto_advance.enabled,
            auto_advance_epoch: 0,
            auto_advance_pending: None,
            recap: Vec::new(),
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
    pub fn play<F: TunnelFinder, S: ScheduleMessageFn>(&mut self, schedule_message: S, tick: Tick, tunnel_finder: F) {
        if let Some(slide) = self.fuiz_config.slides.first() {
            if let Some(team_manager) = &mut self.team_manager
                && matches!(self.state, State::WaitingScreen)
            {
                team_manager.finalize(
                    &mut self.watchers,
                    &mut self.names,
                    tick,
                    &tunnel_finder,
                    self.options.profanity,
                );
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
                tick,
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
    pub fn finish_slide<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
    ) {
        if let State::Slide(current_slide) = &self.state {
            if let Some(mut recap) = current_slide.state.recap(current_slide.index) {
                recap.number = self.recap.len() + 1;
                self.announce_recap_entry(&recap, &tunnel_finder);
                self.recap.push(recap);
            }

            // Opinion and info slides score nothing, so a standings screen after
            // them would just repeat the previous one, so skip straight ahead.
            if self.options.no_leaderboard || !current_slide.state.awards_points() {
                let next_index = current_slide.index + 1;
                if let Some(next_slide) = self.fuiz_config.slides.get(next_index) {
                    let mut state = next_slide.to_state();

                    state.play(
                        self.team_manager.as_ref(),
                        &self.watchers,
                        schedule_message,
                        tick,
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
                                .map(|(id, points)| (self.names.get_name_or_unknown(&id).to_owned(), points))
                                .collect(),
                            team_mapping: self
                                .team_manager
                                .as_ref()
                                .map_or(vec![], |tm| tm.team_assignments(&self.names)),
                            config: &self.fuiz_config,
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
                        config: &self.fuiz_config,
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
    fn handle_unassigned<F: TunnelFinder>(&mut self, watcher: Id, tick: Tick, tunnel_finder: F) {
        if let Some(name_style) = self.options.random_names {
            // One generator for the whole loop: a replay redrawing from a fresh
            // one per attempt would diverge the moment a name collides.
            let mut rng = tick.rng();
            loop {
                let name = name_style.get_name(&mut rng);

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
        let name = self.names.set_name(watcher, name, self.options.profanity)?;

        self.watchers
            .update_watcher_value(watcher, Value::Player(watcher::PlayerValue::Individual));

        self.update_player_with_name(watcher, name, tunnel_finder);

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
            Watchers::send_message(&UpdateMessage::FindTeam(&name).into(), watcher, &tunnel_finder);
        }

        Watchers::send_message(&UpdateMessage::NameAssign(name).into(), watcher, &tunnel_finder);

        self.update_player_with_options(watcher, &tunnel_finder);

        // A player joining mid-game sees the slides they missed, the same as
        // they would after a reconnect.
        Watchers::send_state(
            &SyncMessage::Recap(self.recap_entries(watcher)).into(),
            watcher,
            &tunnel_finder,
        );

        if !name.is_empty() {
            // Announce to others of user joining
            if matches!(self.state, State::WaitingScreen) {
                self.watchers.announce_specific(
                    ValueKind::Host,
                    &UpdateMessage::PlayerJoined(name).into(),
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

    /// Resolves a `SearchTeammate` query for `watcher_id`. Sends back either
    /// [`UpdateMessage::TeammateSelected`] (exact name match, or unique
    /// substring match) or [`UpdateMessage::TeammateSuggestions`] (0 or 2+
    /// substring matches). Self-picks and over-cap selections are dropped.
    fn handle_search_teammate<F: TunnelFinder>(&mut self, watcher_id: Id, query: &str, tunnel_finder: F) {
        let Some(team_manager) = self.team_manager.as_ref() else {
            return;
        };
        if team_manager.is_random_assignments() || !matches!(self.state, State::WaitingScreen) {
            return;
        }
        let max_selection = team_manager.optimal_size.saturating_sub(1);

        let exact = self.names.get_id(query).filter(|id| *id != watcher_id);
        let (teammate_id, name) = if let Some(id) = exact {
            (id, query.to_owned())
        } else {
            let matches = self.names.prefix_search(query, watcher_id, 5);
            if matches.len() == 1 {
                let name = matches[0].to_owned();
                let Some(id) = self.names.get_id(&name) else {
                    return;
                };
                (id, name)
            } else {
                Watchers::send_message(
                    &UpdateMessage::TeammateSuggestions { suggestions: matches }.into(),
                    watcher_id,
                    &tunnel_finder,
                );
                return;
            }
        };

        let already_selected = team_manager.has_preference(watcher_id, teammate_id);
        if !already_selected && team_manager.preference_count(watcher_id) >= max_selection {
            return;
        }
        if !already_selected && let Some(team_manager) = self.team_manager.as_mut() {
            team_manager.add_preference(watcher_id, teammate_id);
        }
        Watchers::send_message(
            &UpdateMessage::TeammateSelected { name: &name }.into(),
            watcher_id,
            &tunnel_finder,
        );
    }

    /// Removes `name` from `watcher_id`'s preference list (if present) and
    /// confirms back to the caller. Idempotent.
    fn handle_deselect_teammate<F: TunnelFinder>(&mut self, watcher_id: Id, name: &str, tunnel_finder: F) {
        let Some(team_manager) = self.team_manager.as_ref() else {
            return;
        };
        if team_manager.is_random_assignments() || !matches!(self.state, State::WaitingScreen) {
            return;
        }
        if let Some(teammate_id) = self.names.get_id(name)
            && let Some(team_manager) = self.team_manager.as_mut()
        {
            team_manager.remove_preference(watcher_id, teammate_id);
        }
        Watchers::send_message(
            &UpdateMessage::TeammateDeselected { name }.into(),
            watcher_id,
            &tunnel_finder,
        );
    }

    // Network

    /// Adds a new unassigned participant to the game and begins name assignment.
    ///
    /// # Arguments
    ///
    /// * `watcher` - Unique ID for the new participant
    /// * `tunnel_finder` - Function to find communication tunnels for participants
    ///
    /// # Type Parameters
    ///
    /// * `T` - Type implementing the Tunnel trait for participant communication
    /// * `F` - Function type for finding tunnels by participant ID
    ///
    /// # Errors
    ///
    /// Returns [`watcher::Error::Locked`] if the game is locked or
    /// [`watcher::Error::MaximumPlayers`] if it is full. In either case the
    /// watcher is not added, so the caller should refuse the connection.
    pub fn add_unassigned<F: TunnelFinder>(
        &mut self,
        watcher: Id,
        tick: Tick,
        tunnel_finder: F,
    ) -> Result<(), watcher::Error> {
        if self.locked {
            return Err(watcher::Error::Locked);
        }

        self.watchers.add_watcher(watcher, Value::Unassigned)?;
        self.handle_unassigned(watcher, tick, tunnel_finder);

        Ok(())
    }

    /// The host-facing screen currently displayed, used to validate an
    /// incoming [`IncomingHostMessage::Next`] against the live game state.
    fn current_host_screen(&self) -> HostScreen {
        match &self.state {
            State::WaitingScreen | State::TeamDisplay => HostScreen::Lobby,
            State::Slide(current_slide) => HostScreen::Slide(current_slide.state.host_position(current_slide.index)),
            State::Leaderboard(index) => HostScreen::Leaderboard { index: *index },
            State::Done => HostScreen::Summary,
        }
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
        tick: Tick,
        tunnel_finder: F,
    ) {
        let Some(watcher_kind) = self.watchers.get_watcher_value_ref(watcher_id).map(Value::kind) else {
            return;
        };

        if !message.follows(watcher_kind) {
            return;
        }

        // Ignore a "Next" whose screen the game has already advanced past,
        // e.g. a double-click issued before the new screen rendered. Acting on
        // it would advance the slide twice (skipping the answering window).
        if let IncomingMessage::Host(IncomingHostMessage::Next(screen)) = &message
            && *screen != self.current_host_screen()
        {
            return;
        }

        let before = (self.current_host_screen(), self.auto_advance_running);
        let mut scheduled = None;
        self.handle_message(
            watcher_id,
            message,
            |alarm, delay| scheduled = Some((alarm, delay)),
            tick,
            &tunnel_finder,
        );
        self.schedule_alarm(before, scheduled, schedule_message, tick, &tunnel_finder);
    }

    /// Acts on a message [`Self::receive_message`] has already validated.
    fn handle_message<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        watcher_id: Id,
        message: IncomingMessage,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
    ) {
        match message {
            IncomingMessage::Unassigned(_) if self.locked => {}
            IncomingMessage::Host(IncomingHostMessage::Next(_)) => {
                self.advance(schedule_message, tick, tunnel_finder);
            }
            IncomingMessage::Host(IncomingHostMessage::Lock(lock_state)) => {
                self.locked = lock_state;
            }
            IncomingMessage::Host(IncomingHostMessage::AutoAdvance(running)) => {
                if running == self.auto_advance_running {
                    return;
                }
                self.auto_advance_running = running;
                self.auto_advance_epoch += 1;
                if let Some((alarm, delay)) = self.auto_advance_alarm() {
                    schedule_message(alarm, delay);
                }
            }
            IncomingMessage::Host(IncomingHostMessage::Kick(target_name)) => {
                if let Some(target_id) = self.names.get_id(&target_name) {
                    self.kick_watcher(target_id, &tunnel_finder);
                }
            }
            IncomingMessage::Host(IncomingHostMessage::RequestResponses) => {
                self.send_player_responses(watcher_id, &tunnel_finder);
            }
            IncomingMessage::Host(IncomingHostMessage::RequestTeamRosters) => {
                self.send_team_rosters(watcher_id, &tunnel_finder);
            }
            IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest(s))
                if self.options.random_names.is_none() =>
            {
                if let Err(e) = self.assign_player_name(watcher_id, &s, &tunnel_finder) {
                    Watchers::send_message(&UpdateMessage::NameError(e).into(), watcher_id, tunnel_finder);
                }
            }
            IncomingMessage::Player(IncomingPlayerMessage::SearchTeammate(query)) => {
                self.handle_search_teammate(watcher_id, &query, &tunnel_finder);
            }
            IncomingMessage::Player(IncomingPlayerMessage::DeselectTeammate(name)) => {
                self.handle_deselect_teammate(watcher_id, &name, &tunnel_finder);
            }
            message => {
                if let State::Slide(current_slide) = &mut self.state
                    && let SlideAction::Next { schedule_message, tick } = current_slide.state.receive_message(
                        &mut self.leaderboard,
                        &self.watchers,
                        self.team_manager.as_ref(),
                        schedule_message,
                        tick,
                        watcher_id,
                        &tunnel_finder,
                        message,
                        current_slide.index,
                        self.fuiz_config.len(),
                    )
                {
                    self.finish_slide(schedule_message, tick, tunnel_finder);
                }
            }
        }
    }

    /// Moves past the current screen. What the host's "Next" does, and what
    /// an [`AutoAdvanceAlarm`] does on the host's behalf.
    fn advance<F: TunnelFinder, S: ScheduleMessageFn>(&mut self, schedule_message: S, tick: Tick, tunnel_finder: F) {
        match &mut self.state {
            State::WaitingScreen | State::TeamDisplay => {
                self.play(schedule_message, tick, &tunnel_finder);
            }
            State::Slide(current_slide) => {
                if let SlideAction::Next { schedule_message, tick } = current_slide.state.receive_host_next(
                    &mut self.leaderboard,
                    &self.watchers,
                    self.team_manager.as_ref(),
                    schedule_message,
                    tick,
                    &tunnel_finder,
                    current_slide.index,
                    self.fuiz_config.len(),
                ) {
                    self.finish_slide(schedule_message, tick, tunnel_finder);
                }
            }
            State::Leaderboard(index) => {
                let next_index = *index + 1;
                if let Some(slide) = self.fuiz_config.slides.get(next_index) {
                    let mut state = slide.to_state();

                    state.play(
                        self.team_manager.as_ref(),
                        &self.watchers,
                        schedule_message,
                        tick,
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
            State::Done => {
                self.mark_as_done(tunnel_finder);
            }
        }
    }

    /// The alarm that advances the current screen, when that screen waits for
    /// the host and is set to auto-advance, and the host hasn't paused it.
    fn auto_advance_alarm(&self) -> Option<(AlarmMessage, Duration)> {
        if !self.auto_advance_running {
            return None;
        }

        let delays = self.options.auto_advance;
        let delay = match &self.state {
            State::TeamDisplay => delays.team_display,
            State::Slide(current_slide) if current_slide.state.in_last_phase() => match &current_slide.state {
                // An info slide with a duration of its own leaves on its own timer.
                SlideState::InfoSlide(info_slide) if info_slide.duration().is_some() => None,
                SlideState::InfoSlide(_) => delays.info_slide,
                _ => delays.results,
            },
            State::Leaderboard(_) => delays.leaderboard,
            State::WaitingScreen | State::Slide(_) | State::Done => None,
        }?;

        Some((
            AutoAdvanceAlarm {
                screen: self.current_host_screen(),
                epoch: self.auto_advance_epoch,
            }
            .into(),
            delay,
        ))
    }

    /// Passes on the one alarm a handler leaves behind: the one it scheduled
    /// itself or, failing that, the auto-advance for a screen the game has
    /// just arrived at. One at most, because a host may hold a single alarm
    /// at a time, a later one replacing the earlier.
    ///
    /// `before` is the screen, and whether auto-advance was on, from before the
    /// handler ran. The host hears about a countdown that starts, and about
    /// auto-advance being switched.
    fn schedule_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        (screen_before, running_before): (HostScreen, bool),
        scheduled: Option<(AlarmMessage, Duration)>,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
    ) {
        let scheduled = scheduled.or_else(|| {
            if self.current_host_screen() == screen_before {
                None
            } else {
                self.auto_advance_alarm()
            }
        });

        let counting_down =
            if let Some((AlarmMessage::AutoAdvance(AutoAdvanceAlarm { screen, .. }), delay)) = &scheduled {
                self.auto_advance_pending = Some(PendingAutoAdvance {
                    screen: *screen,
                    since: tick.now(),
                    delay: *delay,
                });
                true
            } else {
                false
            };

        if counting_down || self.auto_advance_running != running_before {
            self.watchers.announce_specific(
                ValueKind::Host,
                &UpdateMessage::AutoAdvance(self.auto_advance_status()).into(),
                tunnel_finder,
            );
        }

        if let Some((alarm, delay)) = scheduled {
            schedule_message(alarm, delay);
        }
    }

    /// Whether auto-advance is on, and the countdown on the current screen.
    fn auto_advance_status(&self) -> AutoAdvanceStatus {
        let next = self
            .auto_advance_pending
            .filter(|pending| self.auto_advance_running && pending.screen == self.current_host_screen())
            .map(|pending| NextAdvance {
                screen: pending.screen,
                // Read-only, so the live clock is fine: nothing here is stored.
                remaining: pending.delay.saturating_sub(pending.since.elapsed()),
                delay: pending.delay,
            });

        AutoAdvanceStatus {
            running: self.auto_advance_running,
            next,
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
        tick: Tick,
        tunnel_finder: F,
    ) {
        let before = (self.current_host_screen(), self.auto_advance_running);
        let mut scheduled = None;
        self.handle_alarm(
            message,
            |alarm, delay| scheduled = Some((alarm, delay)),
            tick,
            &tunnel_finder,
        );
        self.schedule_alarm(before, scheduled, schedule_message, tick, &tunnel_finder);
    }

    /// Acts on an alarm for [`Self::receive_alarm`].
    fn handle_alarm<F: TunnelFinder, S: ScheduleMessageFn>(
        &mut self,
        message: &AlarmMessage,
        schedule_message: S,
        tick: Tick,
        tunnel_finder: F,
    ) {
        let slide_index = match message {
            AlarmMessage::AutoAdvance(AutoAdvanceAlarm { screen, epoch }) => {
                if self.auto_advance_running
                    && *epoch == self.auto_advance_epoch
                    && *screen == self.current_host_screen()
                {
                    self.advance(schedule_message, tick, tunnel_finder);
                }
                return;
            }
            AlarmMessage::MultipleChoice(multiple_choice::AlarmMessage { index, to: _ })
            | AlarmMessage::TypeAnswer(type_answer::AlarmMessage { index, to: _ })
            | AlarmMessage::Order(order::AlarmMessage { index, to: _ })
            | AlarmMessage::Slider(slider::AlarmMessage { index, to: _ })
            | AlarmMessage::Scale(scale::AlarmMessage { index, to: _ })
            | AlarmMessage::Poll(poll::AlarmMessage { index, to: _ })
            | AlarmMessage::Pin(pin::AlarmMessage { index, to: _ })
            | AlarmMessage::FreeText(free_text::AlarmMessage { index, to: _ })
            | AlarmMessage::Brainstorm(brainstorm::AlarmMessage { index, to: _ })
            | AlarmMessage::InfoSlide(info_slide::AlarmMessage { index, to: _ }) => *index,
        };

        match &mut self.state {
            State::Slide(current_slide) if current_slide.index == slide_index => {
                if let SlideAction::Next { schedule_message, tick } = current_slide.state.receive_alarm(
                    &mut self.leaderboard,
                    &self.watchers,
                    self.team_manager.as_ref(),
                    schedule_message,
                    tick,
                    &tunnel_finder,
                    message,
                    current_slide.index,
                    self.fuiz_config.len(),
                ) {
                    self.finish_slide(schedule_message, tick, tunnel_finder);
                }
            }
            _ => (),
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
    ) -> super::SyncMessage<'_> {
        match &self.state {
            State::WaitingScreen => match &self.team_manager {
                Some(team_manager)
                    if !team_manager.is_random_assignments() && matches!(watcher_kind, ValueKind::Player) =>
                {
                    let selected: Vec<&str> = team_manager
                        .get_preferences(watcher_id)
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|id| self.names.get_name(id))
                        .collect();
                    SyncMessage::TeammatePicker {
                        max_selection: team_manager.optimal_size.saturating_sub(1),
                        selected,
                    }
                    .into()
                }
                // Only the host is told who has joined; everyone else is told
                // how many, matching where `PlayerJoined` is announced.
                _ if matches!(watcher_kind, ValueKind::Host) => {
                    SyncMessage::WaitingScreen(self.waiting_screen_names(tunnel_finder)).into()
                }
                _ => SyncMessage::WaitingCount(self.watchers.specific_count(ValueKind::Player)).into(),
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
                            .map(|(id, points)| (self.names.get_name_or_unknown(&id).to_owned(), points))
                            .collect(),
                        team_mapping: self
                            .team_manager
                            .as_ref()
                            .map_or(vec![], |tm| tm.team_assignments(&self.names)),
                        config: &self.fuiz_config,
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
                    config: &self.fuiz_config,
                })
                .into(),
                ValueKind::Unassigned => SyncMessage::NotAllowed.into(),
            },
        }
    }

    /// Forcibly remove a participant from the game.
    ///
    /// Unlike [`Self::watcher_left`], the participant cannot reconnect: their
    /// id is dropped from the watcher mapping and their name is freed. A
    /// [`UpdateMessage::Kicked`] is delivered before the tunnel is closed so
    /// the client can react. The host cannot be kicked.
    pub fn kick_watcher<F: TunnelFinder>(&mut self, watcher_id: Id, tunnel_finder: F) {
        let kind = match self.watchers.get_watcher_value_ref(watcher_id).map(Value::kind) {
            Some(ValueKind::Host) | None => return,
            Some(kind) => kind,
        };

        if matches!(kind, ValueKind::Player) {
            if let State::Slide(current_slide) = &mut self.state {
                current_slide.state.mark_watcher_left(watcher_id);
            }
            if matches!(self.state, State::WaitingScreen)
                && let Some(name) = self.names.get_name(&watcher_id)
            {
                self.watchers.announce_specific(
                    ValueKind::Host,
                    &UpdateMessage::PlayerLeft(name).into(),
                    &tunnel_finder,
                );
            }
        }

        Watchers::send_message(&UpdateMessage::Kicked.into(), watcher_id, &tunnel_finder);
        Watchers::remove_watcher_session(watcher_id, &tunnel_finder);

        if let Some(team_manager) = &mut self.team_manager {
            team_manager.remove_player(watcher_id);
        }
        self.names.remove_name(&watcher_id);
        self.watchers.remove_watcher(watcher_id);
    }

    /// Notify the game that a watcher's underlying session is gone.
    ///
    /// Drops them from the watcher live set and updates the active slide's
    /// answer tally so subsequent O(1) "all answered" checks stay correct.
    /// The watcher's role is preserved in `mapping` so they can reconnect via
    /// [`Self::update_session`].
    pub fn watcher_left<F: TunnelFinder>(&mut self, watcher_id: Id, tunnel_finder: F) {
        let kind = self.watchers.get_watcher_value_ref(watcher_id).map(Value::kind);
        if matches!(kind, Some(ValueKind::Player)) {
            if let State::Slide(current_slide) = &mut self.state {
                current_slide.state.mark_watcher_left(watcher_id);
            }
            if matches!(self.state, State::WaitingScreen)
                && let Some(name) = self.names.get_name(&watcher_id)
            {
                self.watchers.announce_specific(
                    ValueKind::Host,
                    &UpdateMessage::PlayerLeft(name).into(),
                    &tunnel_finder,
                );
            }
        }
        self.watchers.watcher_left(watcher_id);
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
    pub fn update_session<F: TunnelFinder>(&mut self, watcher_id: Id, tick: Tick, tunnel_finder: F) -> Reconnect {
        // Cheap kind probe: Copy value, borrow ends on this line.
        let Some(kind) = self.watchers.get_watcher_value_ref(watcher_id).map(Value::kind) else {
            return Reconnect::NotFound;
        };

        // Reconnection: re-add to the live set and let the active slide put
        // any prior answer back into its live-answered tally.
        self.watchers.watcher_returned(watcher_id);
        if matches!(kind, ValueKind::Player)
            && let State::Slide(current_slide) = &mut self.state
        {
            current_slide.state.mark_watcher_returned(watcher_id);
        }

        // Re-fetch as a ref now that mutations are done, with no clone.
        let Some(watcher_value) = self.watchers.get_watcher_value_ref(watcher_id) else {
            return Reconnect::NotFound;
        };

        match watcher_value {
            Value::Host => {
                Watchers::send_state(
                    &self.state_message(watcher_id, watcher_value.kind(), &tunnel_finder),
                    watcher_id,
                    &tunnel_finder,
                );
                Watchers::send_state(
                    &SyncMessage::Metainfo(MetainfoMessage::Host {
                        locked: self.locked,
                        teams: self.team_manager.is_some(),
                        auto_advance: self.auto_advance_status(),
                    })
                    .into(),
                    watcher_id,
                    tunnel_finder,
                );
            }
            Value::Player(player_value) => {
                if let PlayerValue::Team { team_name, team_id: _ } = player_value {
                    Watchers::send_message(&UpdateMessage::FindTeam(team_name).into(), watcher_id, &tunnel_finder);
                }
                let player_name = self.names.get_name(&watcher_id).unwrap_or_default().to_owned();
                Watchers::send_message(
                    &UpdateMessage::NameAssign(&player_name).into(),
                    watcher_id,
                    &tunnel_finder,
                );
                if matches!(self.state, State::WaitingScreen) {
                    self.watchers.announce_specific(
                        ValueKind::Host,
                        &UpdateMessage::PlayerJoined(&player_name).into(),
                        &tunnel_finder,
                    );
                }
                self.update_player_with_options(watcher_id, &tunnel_finder);
                Watchers::send_state(
                    &SyncMessage::Recap(self.recap_entries(watcher_id)).into(),
                    watcher_id,
                    &tunnel_finder,
                );
                Watchers::send_state(
                    &self.state_message(watcher_id, ValueKind::Player, &tunnel_finder),
                    watcher_id,
                    &tunnel_finder,
                );
            }
            Value::Unassigned if self.locked => {}
            Value::Unassigned => {
                self.handle_unassigned(watcher_id, tick, &tunnel_finder);
            }
        }

        Reconnect::Reclaimed
    }

    /// Re-attach a connection to the game under `watcher_id`.
    ///
    /// Reclaims the existing watcher when one still holds the id; otherwise
    /// (e.g. after a kick) admits a fresh unassigned participant under the same
    /// id so the client is prompted to pick a name instead of being stranded
    /// with no instructions. The id's origin, a server-assigned value or a
    /// client-supplied routing tag, stays the transport's concern.
    ///
    /// Reclaiming an existing watcher always succeeds.
    ///
    /// # Errors
    ///
    /// Returns the fresh-admission error (the game is locked or at maximum
    /// players) so callers can close the connection, matching how a brand-new
    /// join is handled. Reclaiming an existing watcher never errors.
    pub fn rejoin<F: TunnelFinder>(
        &mut self,
        watcher_id: Id,
        tick: Tick,
        tunnel_finder: F,
    ) -> Result<(), watcher::Error> {
        if matches!(
            self.update_session(watcher_id, tick, &tunnel_finder),
            Reconnect::NotFound
        ) {
            self.add_unassigned(watcher_id, tick, tunnel_finder)
        } else {
            Ok(())
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

    #[cfg(feature = "serializable")]
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
            profanity: Profanity::Censor,
            auto_advance: AutoAdvance::default(),
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
            profanity: Profanity::Censor,
            auto_advance: AutoAdvance::default(),
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
            profanity: Profanity::Censor,
            auto_advance: AutoAdvance::default(),
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
            profanity: Profanity::Censor,
            auto_advance: AutoAdvance::default(),
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
        let current_data = [("Player1", 100u64)];
        let prior_data = [("Player1", 50u64)];

        let leaderboard_msg = LeaderboardMessage {
            current: current_data.to_vec(),
            prior: prior_data.to_vec(),
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

        assert!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder)
                .is_ok()
        );

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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());

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
        let host_message_json =
            r#"{"Host": {"Next": {"Slide": {"MultipleChoice": {"index": 2, "phase": "Answers"}}}}}"#;
        let host_message: IncomingMessage = serde_json::from_str(host_message_json).unwrap();
        assert!(matches!(
            host_message,
            IncomingMessage::Host(IncomingHostMessage::Next(HostScreen::Slide(
                SlidePosition::MultipleChoice {
                    index: 2,
                    phase: crate::fuiz::multiple_choice::Phase::Answers,
                }
            )))
        ));

        // Lobby / Summary screens serialize as bare unit variants.
        let lobby_json = r#"{"Host": {"Next": "Lobby"}}"#;
        let lobby_message: IncomingMessage = serde_json::from_str(lobby_json).unwrap();
        assert!(matches!(
            lobby_message,
            IncomingMessage::Host(IncomingHostMessage::Next(HostScreen::Lobby))
        ));

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

        game.play(&mut schedule_message, Tick::default(), tunnel_finder);

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
        game.play(schedule_message, Tick::default(), tunnel_finder);

        // Finish the slide
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);

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

        // In the lobby a player is told how many have joined, and the host who.
        let player_msg = game.state_message(player_id, crate::watcher::ValueKind::Player, tunnel_finder);
        assert!(matches!(
            player_msg,
            crate::SyncMessage::Game(SyncMessage::WaitingCount(_))
        ));

        let host_msg = game.state_message(host_id, crate::watcher::ValueKind::Host, tunnel_finder);
        assert!(matches!(
            host_msg,
            crate::SyncMessage::Game(SyncMessage::WaitingScreen(_))
        ));
    }

    #[test]
    fn test_incoming_message_follows() {
        // Test IncomingMessage::follows method
        let host_msg = IncomingMessage::Host(IncomingHostMessage::Next(HostScreen::Lobby));
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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());

        // Assign names to make them players
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        // Start the game - should move to team display
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.play(schedule_message, Tick::default(), tunnel_finder);

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

        // A locked game refuses new participants, just like a full one.
        assert_eq!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder),
            Err(crate::watcher::Error::Locked)
        );
        assert!(!game.watchers.has_watcher(unassigned_id));

        // Process lock message from host
        let lock_msg = IncomingMessage::Host(IncomingHostMessage::Lock(false));
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.receive_message(host_id, lock_msg, schedule_message, Tick::default(), tunnel_finder);
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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Try to send host message from player (should be ignored)
        let invalid_msg = IncomingMessage::Host(IncomingHostMessage::Next(HostScreen::Lobby));
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.receive_message(player_id, invalid_msg, schedule_message, Tick::default(), tunnel_finder);

        // State should not change since message doesn't follow sender type
        assert!(matches!(game.state, State::WaitingScreen));
    }

    #[test]
    fn test_game_kick_player_from_waiting_screen() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let host_tunnel = MockTunnel::new();
        let player_id = crate::watcher::Id::new();
        let player_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id {
                Some(host_tunnel.clone())
            } else if id == player_id {
                Some(player_tunnel.clone())
            } else {
                None
            }
        };

        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "Victim", tunnel_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::Kick("Victim".to_owned())),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

        // Watcher fully removed; reconnect attempts hit `has_watcher` and miss.
        assert!(!game.watchers.has_watcher(player_id));
        // Name freed so someone else can claim it.
        let new_id = crate::watcher::Id::new();
        assert!(game.add_unassigned(new_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(new_id, "Victim", tunnel_finder).is_ok());

        let player_msgs = player_tunnel.messages.lock().unwrap();
        assert!(player_msgs.iter().any(|m| m.contains("Kicked")));
        let host_msgs = host_tunnel.messages.lock().unwrap();
        assert!(host_msgs.iter().any(|m| m.contains("PlayerLeft")));
    }

    #[test]
    fn test_game_rejoin_after_kick_admits_fresh_player() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let player_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id {
                Some(player_tunnel.clone())
            } else {
                None
            }
        };

        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "Victim", tunnel_finder).is_ok());
        game.kick_watcher(player_id, tunnel_finder);
        assert!(!game.watchers.has_watcher(player_id));

        // The client still holds its old id and reconnects with it. Rather than
        // a silent no-op, rejoin re-admits it as a fresh unnamed participant.
        assert!(game.rejoin(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.watchers.has_watcher(player_id));
        assert_eq!(
            game.watchers.get_watcher_value(player_id).map(|v| v.kind()),
            Some(crate::watcher::ValueKind::Unassigned)
        );

        // They are prompted to choose a name instead of being stranded.
        let player_msgs = player_tunnel.messages.lock().unwrap();
        assert!(player_msgs.iter().any(|m| m.contains("NameChoose")));
    }

    #[test]
    fn test_game_rejoin_locked_game_is_refused() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;

        game.locked = true;

        // A kicked/new client reconnecting to a locked game is refused, just
        // like one hitting the player cap, so the transport closes it.
        assert_eq!(
            game.rejoin(player_id, Tick::default(), tunnel_finder),
            Err(crate::watcher::Error::Locked)
        );
        assert!(!game.watchers.has_watcher(player_id));
    }

    /// The host's response list is the only place a name and an answer meet:
    /// the slide knows what was answered but not by whom, and the game knows the
    /// names but not the answers. This walks the whole path (join, name, answer,
    /// request) and checks the reply carries both.
    #[test]
    fn requested_responses_name_the_players_who_answered() {
        let fuiz = create_test_fuiz();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, Options::default(), host_id, &test_settings());

        let host_tunnel = MockTunnel::new();
        let player_id = crate::watcher::Id::new();
        let player_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id {
                Some(host_tunnel.clone())
            } else if id == player_id {
                Some(player_tunnel.clone())
            } else {
                None
            }
        };

        game.watchers
            .add_watcher(host_id, crate::watcher::Value::Host)
            .expect("host joins");
        game.watchers
            .add_watcher(player_id, crate::watcher::Value::Unassigned)
            .expect("player joins");
        game.assign_player_name(player_id, "Ada", tunnel_finder)
            .expect("name is free");

        // Start the question and answer it.
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.play(schedule_message, Tick::default(), tunnel_finder);
        let screen = game.current_host_screen();
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::Next(screen)),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );
        game.receive_message(
            player_id,
            IncomingMessage::Player(IncomingPlayerMessage::IndexAnswer(0)),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::RequestResponses),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

        let sent = host_tunnel.messages.lock().unwrap().clone();
        let responses = sent
            .iter()
            .find(|message| message.contains("PlayerResponses"))
            .expect("the host is told who answered");
        assert!(responses.contains("Ada"), "the answer is attributed: {responses}");

        // And a player who never answered is never listed.
        assert!(!responses.contains("Unknown"), "only real names: {responses}");
    }

    #[test]
    fn finished_slides_reach_the_players_recap() {
        let fuiz = create_test_fuiz();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, Options::default(), host_id, &test_settings());

        let host_tunnel = MockTunnel::new();
        let player_id = crate::watcher::Id::new();
        let player_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id {
                Some(host_tunnel.clone())
            } else if id == player_id {
                Some(player_tunnel.clone())
            } else {
                None
            }
        };

        game.watchers
            .add_watcher(host_id, crate::watcher::Value::Host)
            .expect("host joins");
        game.watchers
            .add_watcher(player_id, crate::watcher::Value::Unassigned)
            .expect("player joins");
        game.assign_player_name(player_id, "Ada", tunnel_finder)
            .expect("name is free");

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        let next = |game: &mut Game| {
            let screen = game.current_host_screen();
            game.receive_message(
                host_id,
                IncomingMessage::Host(IncomingHostMessage::Next(screen)),
                schedule_message,
                Tick::default(),
                tunnel_finder,
            );
        };

        game.play(schedule_message, Tick::default(), tunnel_finder);
        next(&mut game);
        game.receive_message(
            player_id,
            IncomingMessage::Player(IncomingPlayerMessage::IndexAnswer(1)),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

        // Nothing reaches the recap while the slide is still on.
        let has_entry = || {
            player_tunnel
                .messages
                .lock()
                .unwrap()
                .iter()
                .any(|message| message.contains("RecapEntry"))
        };
        assert!(!has_entry());

        while matches!(game.state, State::Slide(_)) {
            next(&mut game);
        }

        let sent = player_tunnel.messages.lock().unwrap().clone();
        let entry = sent
            .iter()
            .find(|message| message.contains("RecapEntry"))
            .expect("the player is sent the finished slide");
        assert!(entry.contains(r#""number":1"#), "{entry}");
        assert!(entry.contains(r#""question":"Test Question""#), "{entry}");
        assert!(entry.contains(r#""answer":"Wrong Answer""#), "{entry}");
        assert!(entry.contains(r#""correct_answer":"Correct Answer""#), "{entry}");
        assert!(entry.contains(r#""verdict":"Incorrect""#), "{entry}");
        assert!(entry.contains(r#""points":0"#), "{entry}");

        // A reconnecting player is sent the whole recap again.
        player_tunnel.messages.lock().unwrap().clear();
        assert!(matches!(
            game.update_session(player_id, Tick::default(), tunnel_finder),
            Reconnect::Reclaimed
        ));
        let sent = player_tunnel.messages.lock().unwrap().clone();
        let recap = sent
            .iter()
            .find(|message| message.contains(r#""Recap":["#))
            .expect("the reconnecting player is sent their recap");
        assert!(recap.contains(r#""answer":"Wrong Answer""#), "{recap}");
    }

    /// Team options carry private fields, so build them the way a request does.
    fn team_options(size: usize) -> Options {
        serde_json::from_str(&format!(
            r#"{{ "random_names": null, "show_answers": true, "no_leaderboard": false,
                  "teams": {{ "size": {size}, "assign_random": true }}, "profanity": "Allow" }}"#
        ))
        .expect("team options deserialize")
    }

    /// The roster list is the only thing that tells the host who is on which
    /// team while the game is running: the team display carries names alone,
    /// and the summary's mapping does not arrive until the game is over. It is
    /// also the only report of who has dropped, since a player leaving
    /// mid-game raises no host-facing event.
    #[test]
    fn requested_rosters_name_members_and_flag_the_disconnected() {
        let fuiz = create_test_fuiz();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, team_options(2), host_id, &test_settings());

        let host_tunnel = MockTunnel::new();
        let stayed = crate::watcher::Id::new();
        let dropped = crate::watcher::Id::new();

        // Everyone is present while the teams form.
        let all_present = |id: crate::watcher::Id| {
            if id == host_id {
                Some(host_tunnel.clone())
            } else if id == stayed || id == dropped {
                Some(MockTunnel::new())
            } else {
                None
            }
        };

        game.watchers
            .add_watcher(host_id, crate::watcher::Value::Host)
            .expect("host joins");
        for (id, name) in [(stayed, "Ada"), (dropped, "Grace")] {
            game.watchers
                .add_watcher(id, crate::watcher::Value::Unassigned)
                .expect("player joins");
            game.assign_player_name(id, name, all_present).expect("name is free");
        }

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.play(schedule_message, Tick::default(), all_present);

        // One socket goes away. The roster keeps the player and reports it.
        let one_dropped = |id: crate::watcher::Id| {
            if id == dropped { None } else { all_present(id) }
        };

        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::RequestTeamRosters),
            schedule_message,
            Tick::default(),
            one_dropped,
        );

        let sent = host_tunnel.messages.lock().unwrap().clone();
        let rosters = sent
            .iter()
            .find(|message| message.contains("TeamRosters"))
            .expect("the host is told who is on each team");

        assert!(rosters.contains("Ada"), "members are named: {rosters}");
        assert!(rosters.contains("Grace"), "a dropped member stays listed: {rosters}");
        assert!(
            rosters.contains(r#"{"name":"Grace","connected":false}"#),
            "the dropped member reads as disconnected: {rosters}"
        );
        assert!(
            rosters.contains(r#"{"name":"Ada","connected":true}"#),
            "the present member reads as connected: {rosters}"
        );
    }

    /// A game without teams still answers, so the host's screen renders an
    /// empty list rather than waiting on a reply that never comes.
    #[test]
    fn requested_rosters_are_empty_without_teams() {
        let fuiz = create_test_fuiz();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, Options::default(), host_id, &test_settings());

        let host_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| (id == host_id).then(|| host_tunnel.clone());

        game.watchers
            .add_watcher(host_id, crate::watcher::Value::Host)
            .expect("host joins");

        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::RequestTeamRosters),
            |_: crate::AlarmMessage, _: std::time::Duration| {},
            Tick::default(),
            tunnel_finder,
        );

        let sent = host_tunnel.messages.lock().unwrap().clone();
        assert!(
            sent.iter().any(|message| message.contains(r#"{"TeamRosters":[]}"#)),
            "an empty list, not silence: {sent:?}"
        );
    }

    /// Rosters are host-only. A player asking gets nothing, including on the
    /// socket path that answers the query without going through the log.
    #[test]
    fn players_cannot_request_rosters() {
        let fuiz = create_test_fuiz();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, team_options(2), host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let player_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| (id == player_id).then(|| player_tunnel.clone());

        game.watchers
            .add_watcher(host_id, crate::watcher::Value::Host)
            .expect("host joins");
        game.watchers
            .add_watcher(player_id, crate::watcher::Value::Unassigned)
            .expect("player joins");
        game.assign_player_name(player_id, "Ada", tunnel_finder)
            .expect("name is free");

        game.receive_message(
            player_id,
            IncomingMessage::Host(IncomingHostMessage::RequestTeamRosters),
            |_: crate::AlarmMessage, _: std::time::Duration| {},
            Tick::default(),
            tunnel_finder,
        );
        // The direct call the Durable Object makes, which skips the kind check
        // that `receive_message` performs.
        game.send_team_rosters(player_id, tunnel_finder);

        let sent = player_tunnel.messages.lock().unwrap().clone();
        assert!(
            !sent.iter().any(|message| message.contains("TeamRosters")),
            "a player never sees the rosters: {sent:?}"
        );
    }

    #[test]
    fn test_game_kick_host_is_noop() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let host_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == host_id { Some(host_tunnel.clone()) } else { None }
        };

        game.kick_watcher(host_id, tunnel_finder);
        assert!(game.watchers.has_watcher(host_id));
    }

    #[test]
    fn test_game_kick_non_host_player_only() {
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let player_id = crate::watcher::Id::new();
        let player_tunnel = MockTunnel::new();
        let tunnel_finder = |id: crate::watcher::Id| {
            if id == player_id {
                Some(player_tunnel.clone())
            } else {
                None
            }
        };

        // Players cannot kick anyone; `follows` rejects the Host message.
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        let other = crate::watcher::Id::new();
        let other_finder = |_: crate::watcher::Id| Some(player_tunnel.clone());
        assert!(game.add_unassigned(other, Tick::default(), other_finder).is_ok());
        assert!(game.assign_player_name(other, "Target", other_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};
        game.receive_message(
            player_id,
            IncomingMessage::Host(IncomingHostMessage::Kick("Target".to_owned())),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );
        assert!(game.watchers.has_watcher(other));
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
        let msg = IncomingMessage::Host(IncomingHostMessage::Next(HostScreen::Lobby));
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Should not panic and should do nothing
        game.receive_message(nonexistent_id, msg, schedule_message, Tick::default(), tunnel_finder);
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
        assert!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder)
                .is_ok()
        );

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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());

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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        // Send teammate selection message via search
        let teammate_msg = IncomingMessage::Player(IncomingPlayerMessage::SearchTeammate("Player2".to_string()));
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.receive_message(player1, teammate_msg, schedule_message, Tick::default(), tunnel_finder);

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
        game.play(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish the slide - should show leaderboard
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Leaderboard(_)));

        // Send next from host while in leaderboard state
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next(game.current_host_screen()));
        game.receive_message(host_id, next_msg, schedule_message, Tick::default(), tunnel_finder);

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
        game.play(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish the slide - should skip leaderboard and go to done
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);
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
        game.play(schedule_message, Tick::default(), tunnel_finder);

        // Send index message from host (should be handled by slide state)
        let index_msg = IncomingMessage::Host(IncomingHostMessage::Index(0));
        game.receive_message(host_id, index_msg, schedule_message, Tick::default(), tunnel_finder);
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
        assert_eq!(
            game.update_session(host_id, Tick::default(), tunnel_finder),
            Reconnect::Reclaimed
        );

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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Update player session - should work with team manager
        assert_eq!(
            game.update_session(player_id, Tick::default(), tunnel_finder),
            Reconnect::Reclaimed
        );
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

        // Join while open, then the host locks the game.
        assert!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder)
                .is_ok()
        );
        game.locked = true;

        // Reconnecting reclaims the existing watcher but sends nothing, since it
        // is still unnamed and the game is now locked.
        assert_eq!(
            game.update_session(unassigned_id, Tick::default(), tunnel_finder),
            Reconnect::Reclaimed
        );
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
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next(game.current_host_screen()));
        game.receive_message(host_id, next_msg, schedule_message, Tick::default(), tunnel_finder);

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
        game.play(schedule_message, Tick::default(), tunnel_finder);

        // Send alarm for wrong slide index - should be ignored
        let alarm = crate::AlarmMessage::MultipleChoice(crate::fuiz::multiple_choice::AlarmMessage {
            index: 999, // Wrong index
            to: crate::fuiz::multiple_choice::Phase::Question,
        });

        game.receive_alarm(&alarm, schedule_message, Tick::default(), tunnel_finder);

        // State should not change (still in slide 0, not 999)
        assert!(matches!(game.state, State::Slide(_)));
    }

    #[test]
    fn test_stale_host_next_is_ignored() {
        // A second "Next" carrying the screen the host saw before the first
        // click took effect must not advance the slide a second time.
        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        game.play(schedule_message, Tick::default(), tunnel_finder);
        let stale_screen = game.current_host_screen();

        // First Next (matching the live screen) advances the phase.
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::Next(stale_screen)),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );
        let advanced_screen = game.current_host_screen();
        assert_ne!(stale_screen, advanced_screen);

        // Second Next carrying the now-stale screen is ignored.
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::Next(stale_screen)),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );
        assert_eq!(game.current_host_screen(), advanced_screen);
    }

    #[test]
    fn test_slide_starts_on_announcement_phase() {
        use crate::fuiz::multiple_choice::Phase;

        let fuiz = create_test_fuiz();
        let options = Options::default();
        let host_id = crate::watcher::Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());

        let tunnel_finder = |_: crate::watcher::Id| None::<MockTunnel>;
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Playing a slide starts on the announcement (`Unstarted`), not the question.
        game.play(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(
            game.current_host_screen(),
            HostScreen::Slide(SlidePosition::MultipleChoice {
                index: 0,
                phase: Phase::Unstarted,
            })
        ));

        // Advancing reveals the question.
        let screen = game.current_host_screen();
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::Next(screen)),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );
        assert!(matches!(
            game.current_host_screen(),
            HostScreen::Slide(SlidePosition::MultipleChoice {
                phase: Phase::Question,
                ..
            })
        ));
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
        game.play(schedule_message, Tick::default(), tunnel_finder);
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

        let name_assign = UpdateMessage::NameAssign("TestName");
        let json = serde_json::to_string(&name_assign).unwrap();
        assert!(json.contains("TestName"));

        let find_team = UpdateMessage::FindTeam("TeamName");
        let json = serde_json::to_string(&find_team).unwrap();
        assert!(json.contains("TeamName"));
    }

    #[test]
    fn test_sync_message_serialization() {
        // Test various SyncMessage variants
        let metainfo_host = SyncMessage::Metainfo(MetainfoMessage::Host {
            locked: true,
            teams: false,
            auto_advance: AutoAdvanceStatus {
                running: false,
                next: None,
            },
        });
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
        let fuiz = create_test_fuiz();
        let player_summary = SummaryMessage::Player {
            score: None,
            points: vec![100, 200],
            config: &fuiz,
        };
        let json = serde_json::to_string(&player_summary).unwrap();
        assert!(json.contains("Player"));
        assert!(json.contains("points"));

        let host_summary = SummaryMessage::Host {
            stats: vec![(5, 10), (3, 8)],
            player_count: 15,
            results: vec![],
            team_mapping: vec![],
            config: &fuiz,
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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // First play call should transition to TeamDisplay
        game.play(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::TeamDisplay));

        // Second play call should start the actual game
        game.play(schedule_message, Tick::default(), tunnel_finder);
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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
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
        game.play(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish first slide - should go to second slide
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish second slide - should be done
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);
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
        game.play(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish first slide - should show leaderboard
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Leaderboard(0)));

        // Send next from host - should advance to slide 1
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next(game.current_host_screen()));
        game.receive_message(host_id, next_msg, schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Slide(_)));

        // Finish second slide - should show leaderboard again
        game.finish_slide(schedule_message, Tick::default(), tunnel_finder);
        assert!(matches!(game.state, State::Leaderboard(1)));

        // Send next from host - should be done (no more slides)
        let next_msg = IncomingMessage::Host(IncomingHostMessage::Next(game.current_host_screen()));
        game.receive_message(host_id, next_msg, schedule_message, Tick::default(), tunnel_finder);
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
        assert!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder)
                .is_ok()
        );
        game.locked = true;

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Try to send name request - should be ignored due to lock
        let name_msg = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("TestName".to_string()));
        game.receive_message(
            unassigned_id,
            name_msg,
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

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
        assert!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder)
                .is_ok()
        );

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // Try to send name request when random names are enabled - should be ignored
        let name_msg = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("TestName".to_string()));
        game.receive_message(
            unassigned_id,
            name_msg,
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());

        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        // First player takes a name
        let name_msg1 = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("SameName".to_string()));
        game.receive_message(player1, name_msg1, schedule_message, Tick::default(), tunnel_finder);

        // Second player tries to take the same name - should get error message
        let name_msg2 = IncomingMessage::Unassigned(IncomingUnassignedMessage::NameRequest("SameName".to_string()));
        game.receive_message(player2, name_msg2, schedule_message, Tick::default(), tunnel_finder);

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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        // Should return teammate selection message for players in waiting screen with non-random teams
        let state_msg = game.state_message(player1, crate::watcher::ValueKind::Player, tunnel_finder);
        assert!(matches!(
            state_msg,
            crate::SyncMessage::Game(SyncMessage::TeammatePicker { .. })
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

        // A nonexistent watcher reports NotFound rather than silently no-opping.
        assert_eq!(
            game.update_session(nonexistent_id, Tick::default(), tunnel_finder),
            Reconnect::NotFound
        );
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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());

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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());

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
    fn test_search_teammate_exact_match_selects() {
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
        let schedule_message = |_: crate::AlarmMessage, _: std::time::Duration| {};

        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        game.receive_message(
            player1,
            IncomingMessage::Player(IncomingPlayerMessage::SearchTeammate("Player2".to_string())),
            schedule_message,
            Tick::default(),
            tunnel_finder,
        );

        let team_manager = game.team_manager.as_ref().expect("team manager configured");
        assert!(team_manager.has_preference(player1, player2));
    }

    #[test]
    fn test_incoming_host_message_index() {
        let index_msg = IncomingHostMessage::Index(5);
        assert!(matches!(index_msg, IncomingHostMessage::Index(5)));

        let lock_msg = IncomingHostMessage::Lock(true);
        assert!(matches!(lock_msg, IncomingHostMessage::Lock(true)));
    }

    #[cfg(feature = "serializable")]
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
            profanity: Profanity::Censor,
            auto_advance: AutoAdvance::default(),
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
            profanity: Profanity::Censor,
            auto_advance: AutoAdvance::default(),
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
        assert!(game.add_unassigned(player1, Tick::default(), tunnel_finder).is_ok());
        assert!(game.add_unassigned(player2, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player1, "Player1", tunnel_finder).is_ok());
        assert!(game.assign_player_name(player2, "Player2", tunnel_finder).is_ok());

        let names = game.waiting_screen_names(tunnel_finder);
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"Player1"));
        assert!(names.contains(&"Player2"));
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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Test leaderboard_message function
        let leaderboard_msg = game.leaderboard_message();
        assert_eq!(leaderboard_msg.current, [] as [(&str, u64); 0]); // No scores yet
        assert_eq!(leaderboard_msg.prior, [] as [(&str, u64); 0]);
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
        game.play(schedule_message, Tick::default(), tunnel_finder);

        // Send alarm for correct slide index
        let alarm = crate::AlarmMessage::MultipleChoice(crate::fuiz::multiple_choice::AlarmMessage {
            index: 0, // Correct index
            to: crate::fuiz::multiple_choice::Phase::Question,
        });

        // Should handle the alarm for the current slide
        game.receive_alarm(&alarm, schedule_message, Tick::default(), tunnel_finder);
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
        let alarm = crate::AlarmMessage::MultipleChoice(crate::fuiz::multiple_choice::AlarmMessage {
            index: 0,
            to: crate::fuiz::multiple_choice::Phase::Question,
        });

        game.receive_alarm(&alarm, schedule_message, Tick::default(), tunnel_finder);
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
        game.play(schedule_message, Tick::default(), tunnel_finder);

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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
        assert!(game.assign_player_name(player_id, "TestPlayer", tunnel_finder).is_ok());

        // Update session for individual player (no teams)
        assert_eq!(
            game.update_session(player_id, Tick::default(), tunnel_finder),
            Reconnect::Reclaimed
        );
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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());
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
        assert!(
            game.add_unassigned(unassigned_id, Tick::default(), tunnel_finder)
                .is_ok()
        );

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
        assert!(game.add_unassigned(player_id, Tick::default(), tunnel_finder).is_ok());

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
        let type_answer_alarm = crate::AlarmMessage::TypeAnswer(crate::fuiz::type_answer::AlarmMessage {
            index: 0,
            to: crate::fuiz::type_answer::Phase::Question,
        });
        assert!(matches!(type_answer_alarm, crate::AlarmMessage::TypeAnswer(_)));

        let order_alarm = crate::AlarmMessage::Order(crate::fuiz::order::AlarmMessage {
            index: 0,
            to: crate::fuiz::order::Phase::Question,
        });
        assert!(matches!(order_alarm, crate::AlarmMessage::Order(_)));
    }

    /// Records every alarm a handler schedules, so a test can fire them later.
    #[derive(Default)]
    struct Scheduled(std::cell::RefCell<Vec<(crate::AlarmMessage, Duration)>>);

    impl Scheduled {
        fn sink(&self) -> impl FnOnce(crate::AlarmMessage, Duration) + '_ {
            |alarm, delay| self.0.borrow_mut().push((alarm, delay))
        }

        /// The one alarm scheduled since the last call, if any.
        fn take(&self) -> Option<(crate::AlarmMessage, Duration)> {
            let mut scheduled = self.0.borrow_mut();
            assert!(scheduled.len() <= 1, "a handler scheduled more than one alarm");
            scheduled.pop()
        }

        /// The one alarm scheduled since the last call, which must be an
        /// auto-advance.
        fn take_auto_advance(&self) -> (crate::AlarmMessage, Duration) {
            let scheduled = self.take().expect("nothing was scheduled");
            assert!(matches!(scheduled.0, crate::AlarmMessage::AutoAdvance(_)));
            scheduled
        }
    }

    fn auto_advance_delays() -> AutoAdvance {
        AutoAdvance {
            enabled: true,
            team_display: Some(Duration::from_secs(2)),
            results: Some(Duration::from_secs(5)),
            leaderboard: Some(Duration::from_secs(3)),
            info_slide: Some(Duration::from_secs(4)),
        }
    }

    fn two_question_fuiz() -> crate::fuiz::config::Fuiz {
        let question = r#"{
            "MultipleChoice": {
                "title": "Question",
                "media": null,
                "introduce_question": 5000000000,
                "time_limit": 30000000000,
                "points_awarded": 1000,
                "answers": [{"correct": true, "content": {"Text": "Answer"}}]
            }
        }"#;
        serde_json::from_str(&format!(r#"{{"title": "Quiz", "slides": [{question}, {question}]}}"#)).unwrap()
    }

    /// Plays the first slide through to its results, the first screen that
    /// waits for the host.
    fn game_on_first_results(options: Options, scheduled: &Scheduled) -> (Game, Id) {
        let host_id = Id::new();
        let mut game = Game::new(two_question_fuiz(), options, host_id, &test_settings());
        let tunnel_finder = |_: Id| None::<MockTunnel>;

        game.play(|_, _| {}, Tick::default(), tunnel_finder);
        while let State::Slide(current_slide) = &game.state
            && !current_slide.state.in_last_phase()
        {
            scheduled.take();
            let next = IncomingMessage::Host(IncomingHostMessage::Next(game.current_host_screen()));
            game.receive_message(host_id, next, scheduled.sink(), Tick::default(), tunnel_finder);
        }
        (game, host_id)
    }

    fn fire(game: &mut Game, (alarm, _): &(crate::AlarmMessage, Duration), scheduled: &Scheduled) {
        game.receive_alarm(alarm, scheduled.sink(), Tick::default(), |_: Id| None::<MockTunnel>);
    }

    fn send_host(game: &mut Game, host_id: Id, message: IncomingHostMessage, scheduled: &Scheduled) {
        game.receive_message(
            host_id,
            IncomingMessage::Host(message),
            scheduled.sink(),
            Tick::default(),
            |_: Id| None::<MockTunnel>,
        );
    }

    fn send_next(game: &mut Game, host_id: Id, scheduled: &Scheduled) {
        let screen = game.current_host_screen();
        send_host(game, host_id, IncomingHostMessage::Next(screen), scheduled);
    }

    #[test]
    fn auto_advance_walks_through_host_paced_screens() {
        let scheduled = Scheduled::default();
        let options = Options::default().with_auto_advance(auto_advance_delays());
        let (mut game, _) = game_on_first_results(options, &scheduled);

        let results_alarm = scheduled.take_auto_advance();
        assert_eq!(results_alarm.1, Duration::from_secs(5));

        fire(&mut game, &results_alarm, &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
        let leaderboard_alarm = scheduled.take_auto_advance();
        assert_eq!(leaderboard_alarm.1, Duration::from_secs(3));

        fire(&mut game, &leaderboard_alarm, &scheduled);
        assert!(matches!(&game.state, State::Slide(current_slide) if current_slide.index == 1));
        // The new slide runs on its own timers, not an auto-advance.
        assert!(!matches!(
            scheduled.take(),
            Some((crate::AlarmMessage::AutoAdvance(_), _))
        ));
    }

    #[test]
    fn auto_advance_is_off_by_default() {
        let scheduled = Scheduled::default();
        let (mut game, host_id) = game_on_first_results(Options::default(), &scheduled);
        assert!(scheduled.take().is_none());

        send_next(&mut game, host_id, &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
        assert!(scheduled.take().is_none());
    }

    #[test]
    fn auto_advance_alarm_is_stale_after_a_manual_next() {
        let scheduled = Scheduled::default();
        let options = Options::default().with_auto_advance(auto_advance_delays());
        let (mut game, host_id) = game_on_first_results(options, &scheduled);
        let results_alarm = scheduled.take_auto_advance();

        send_next(&mut game, host_id, &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
        scheduled.take_auto_advance();

        // The results alarm names a screen the host already left.
        fire(&mut game, &results_alarm, &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
        assert!(scheduled.take().is_none());
    }

    #[test]
    fn paused_auto_advance_holds_until_resumed() {
        let scheduled = Scheduled::default();
        let options = Options::default().with_auto_advance(auto_advance_delays());
        let (mut game, host_id) = game_on_first_results(options, &scheduled);
        let results_alarm = scheduled.take_auto_advance();

        send_host(&mut game, host_id, IncomingHostMessage::AutoAdvance(false), &scheduled);
        assert!(scheduled.take().is_none());
        fire(&mut game, &results_alarm, &scheduled);
        assert!(!matches!(game.state, State::Leaderboard(_)));

        // Resuming schedules afresh; the alarm from before the pause stays dead.
        send_host(&mut game, host_id, IncomingHostMessage::AutoAdvance(true), &scheduled);
        let resumed_alarm = scheduled.take_auto_advance();
        fire(&mut game, &results_alarm, &scheduled);
        assert!(!matches!(game.state, State::Leaderboard(_)));
        fire(&mut game, &resumed_alarm, &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
        scheduled.take_auto_advance();

        // A pause outlasts the screen it was made on.
        send_host(&mut game, host_id, IncomingHostMessage::AutoAdvance(false), &scheduled);
        send_next(&mut game, host_id, &scheduled);
        let State::Slide(current_slide) = &game.state else {
            panic!("expected the second slide");
        };
        assert_eq!(current_slide.index, 1);
        assert!(!matches!(
            scheduled.take(),
            Some((crate::AlarmMessage::AutoAdvance(_), _))
        ));
        assert!(!game.auto_advance_running);
    }

    #[test]
    fn auto_advance_can_be_switched_on_mid_game() {
        let scheduled = Scheduled::default();
        let (mut game, host_id) = game_on_first_results(Options::default(), &scheduled);
        assert!(scheduled.take().is_none());

        send_host(&mut game, host_id, IncomingHostMessage::AutoAdvance(true), &scheduled);
        let (alarm, delay) = scheduled.take_auto_advance();
        assert_eq!(Some(delay), AutoAdvance::default().results);

        fire(&mut game, &(alarm, delay), &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
        assert_eq!(
            Some(scheduled.take_auto_advance().1),
            AutoAdvance::default().leaderboard
        );
    }

    #[test]
    fn host_hears_the_countdown_and_the_switch() {
        let scheduled = Scheduled::default();
        let options = Options::default().with_auto_advance(auto_advance_delays());
        let (mut game, host_id) = game_on_first_results(options, &scheduled);
        scheduled.take_auto_advance();

        let status = game.auto_advance_status();
        assert!(status.running);
        let next = status.next.expect("the results screen is counting down");
        assert_eq!(next.screen, game.current_host_screen());
        assert_eq!(next.delay, Duration::from_secs(5));
        assert!(next.remaining <= next.delay);

        let tunnel = MockTunnel::new();
        let host_tunnel = |id: Id| (id == host_id).then(|| tunnel.clone());
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::AutoAdvance(false)),
            scheduled.sink(),
            Tick::default(),
            host_tunnel,
        );
        let messages = tunnel.messages.lock().unwrap().clone();
        assert_eq!(messages, [r#"{"Game":{"AutoAdvance":{"running":false,"next":null}}}"#]);
        assert!(game.auto_advance_status().next.is_none());

        // Back on, the countdown starts over and the host is told.
        game.receive_message(
            host_id,
            IncomingMessage::Host(IncomingHostMessage::AutoAdvance(true)),
            scheduled.sink(),
            Tick::default(),
            host_tunnel,
        );
        scheduled.take_auto_advance();
        let messages = tunnel.messages.lock().unwrap().clone();
        assert_eq!(messages.len(), 2);
        assert!(messages[1].contains(r#""running":true"#) && messages[1].contains(r#""delay":5000"#));
    }

    #[test]
    fn repeated_resume_keeps_the_pending_alarm() {
        let scheduled = Scheduled::default();
        let options = Options::default().with_auto_advance(auto_advance_delays());
        let (mut game, host_id) = game_on_first_results(options, &scheduled);
        let results_alarm = scheduled.take_auto_advance();

        send_host(&mut game, host_id, IncomingHostMessage::AutoAdvance(true), &scheduled);
        assert!(scheduled.take().is_none());
        fire(&mut game, &results_alarm, &scheduled);
        assert!(matches!(game.state, State::Leaderboard(0)));
    }

    #[test]
    fn info_slide_auto_advances_only_without_a_duration_of_its_own() {
        let fuiz: crate::fuiz::config::Fuiz = serde_json::from_str(
            r#"{"title": "Info", "slides": [
                {"InfoSlide": {"title": "Host-paced", "media": null}},
                {"InfoSlide": {"title": "Timed", "media": null, "duration": 10000}}
            ]}"#,
        )
        .unwrap();
        let options = Options::default().with_auto_advance(auto_advance_delays());
        let host_id = Id::new();
        let mut game = Game::new(fuiz, options, host_id, &test_settings());
        let scheduled = Scheduled::default();
        send_next(&mut game, host_id, &scheduled);

        let info_alarm = scheduled.take_auto_advance();
        assert_eq!(info_alarm.1, Duration::from_secs(4));

        fire(&mut game, &info_alarm, &scheduled);
        assert!(matches!(&game.state, State::Slide(current_slide) if current_slide.index == 1));
        let (own_alarm, delay) = scheduled.take().expect("the timed slide schedules its own alarm");
        assert!(matches!(own_alarm, crate::AlarmMessage::InfoSlide(_)));
        assert_eq!(delay, Duration::from_secs(10));
    }

    #[test]
    fn auto_advance_options_parse_and_validate() {
        let options: Options = serde_json::from_str(
            r#"{"random_names": null, "show_answers": false, "no_leaderboard": false, "teams": null,
                "auto_advance": {"results": 5000, "leaderboard": null}}"#,
        )
        .unwrap();
        assert_eq!(options.auto_advance.results, Some(Duration::from_secs(5)));
        assert_eq!(options.auto_advance.leaderboard, None);
        // Missing fields fall back to the defaults, not to `None`.
        assert_eq!(options.auto_advance.info_slide, AutoAdvance::default().info_slide);
        assert!(!options.auto_advance.enabled);

        let switched_on: Options = serde_json::from_str(
            r#"{"random_names": null, "show_answers": false, "no_leaderboard": false, "teams": null,
                "auto_advance": {"enabled": true}}"#,
        )
        .unwrap();
        assert_eq!(
            switched_on.auto_advance,
            AutoAdvance {
                enabled: true,
                ..AutoAdvance::default()
            }
        );
        assert!(options.validate_with(&test_settings()).is_ok());

        let legacy: Options = serde_json::from_str(
            r#"{"random_names": null, "show_answers": false, "no_leaderboard": false, "teams": null}"#,
        )
        .unwrap();
        assert_eq!(legacy.auto_advance, AutoAdvance::default());

        for out_of_bounds in [Duration::ZERO, Duration::from_secs(241)] {
            let options = Options::default().with_auto_advance(AutoAdvance {
                leaderboard: Some(out_of_bounds),
                ..auto_advance_delays()
            });
            assert!(options.validate_with(&test_settings()).is_err());
        }
    }
}
