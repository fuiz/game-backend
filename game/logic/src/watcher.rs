//! Player and host watcher management
//!
//! This module manages the connections and state of all participants in a game
//! session, including hosts, players, and unassigned connections. It provides
//! functionality for tracking participant types, sending messages, and managing
//! the overall participant lifecycle.

use std::{fmt::Display, hash::BuildHasherDefault, str::FromStr};

use enum_map::{Enum, EnumMap};
use hashlink::LinkedHashSet;
use itertools::Itertools;
use rustc_hash::{FxHashMap, FxHasher};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

/// `LinkedHashSet` keyed by `Id` (random UUID bytes) using FxHash. Avoids the
/// SipHash overhead of the default `RandomState` for keys that already have
/// uniform/random bits.
type FxLinkedHashSet<T> = LinkedHashSet<T, BuildHasherDefault<FxHasher>>;

use super::{
    SyncMessage, UpdateMessage,
    session::{Tunnel, TunnelFinder},
};

/// A unique identifier for participants in the game
///
/// Each participant (host, player, or unassigned connection) gets a unique ID
/// that persists throughout their participation in the game session.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Id(Uuid);

impl Id {
    /// Creates a new random participant ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for Id {
    /// Creates a new random participant ID (same as `new()`)
    fn default() -> Self {
        Self::new()
    }
}

impl Display for Id {
    /// Formats the ID as a UUID string
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for Id {
    type Err = uuid::Error;

    /// Parses an ID from a UUID string
    ///
    /// # Errors
    ///
    /// Returns a `uuid::Error` if the string is not a valid UUID.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::from_str(s)?))
    }
}

/// The different types of participants in a game session
///
/// This enum represents the role and state of each participant,
/// determining what actions they can perform and what information
/// they receive.
/// Represents the type and state of a participant in the game
///
/// This enum distinguishes between different participant types and their roles,
/// determining what actions they can perform and what information they receive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub enum Value {
    /// A connection that hasn't been assigned a role yet
    Unassigned,
    /// The game host who controls the game flow
    Host,
    /// A player participating in the game
    Player(PlayerValue),
}

/// The kind of participant without associated data
///
/// This enum represents just the discriminant of the Value enum,
/// useful for pattern matching and filtering participants by type
/// without needing the associated data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Enum, Serialize, Deserialize)]
pub enum ValueKind {
    /// An unassigned connection
    Unassigned,
    /// A game host
    Host,
    /// A game player
    Player,
}

impl Value {
    /// Returns the kind of this value without the associated data
    ///
    /// # Returns
    ///
    /// The `ValueKind` corresponding to this Value variant
    pub fn kind(&self) -> ValueKind {
        match self {
            Value::Unassigned => ValueKind::Unassigned,
            Value::Host => ValueKind::Host,
            Value::Player(_) => ValueKind::Player,
        }
    }
}

/// Player-specific data and state
///
/// This enum differentiates between individual players and team players,
/// tracking the necessary information for each type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
pub enum PlayerValue {
    /// An individual player not part of a team
    Individual {
        /// The player's chosen name
        name: String,
    },
    /// A player who is part of a team
    Team {
        /// The name of the team
        team_name: String,
        /// The individual player's name within the team
        individual_name: String,
        /// The unique identifier for the team
        team_id: Id,
    },
}

impl PlayerValue {
    /// Gets the individual name of the player
    ///
    /// For individual players, this returns their name.
    /// For team players, this returns their individual name within the team.
    pub fn name(&self) -> &str {
        match self {
            Self::Individual { name } => name,
            Self::Team {
                team_name: _,
                individual_name,
                team_id: _,
            } => individual_name,
        }
    }
}

/// Serialization helper for Watchers struct. Only compiled when persistence
/// is enabled, since `Watchers`' `Deserialize` (via `serde(from = ...)`)
/// references it.
#[cfg(feature = "serializable")]
#[derive(Deserialize)]
struct WatchersSerde {
    mapping: FxHashMap<Id, Value>,
    max_player_count: usize,
}

/// Manages all participants (watchers) in a game session
///
/// This struct tracks all connected participants, their roles, and provides
/// functionality for sending messages, managing state, and organizing
/// participants by type.
#[cfg_attr(feature = "serializable", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serializable", serde(from = "WatchersSerde"))]
pub struct Watchers {
    /// Primary mapping from participant ID to their value/state
    mapping: FxHashMap<Id, Value>,

    /// Reverse mapping organized by participant type for efficient filtering.
    /// Uses `LinkedHashSet` so iteration follows insertion order (letting the
    /// waiting screen surface the most recently joined players) while keeping
    /// O(1) insertion and removal.
    #[cfg_attr(feature = "serializable", serde(skip_serializing))]
    reverse_mapping: EnumMap<ValueKind, FxLinkedHashSet<Id>>,

    /// Maximum number of players allowed in a single game session
    max_player_count: usize,
}

#[cfg(feature = "serializable")]
impl From<WatchersSerde> for Watchers {
    /// Reconstructs the Watchers struct from serialized data
    ///
    /// This rebuilds the reverse mapping from the primary mapping,
    /// which is necessary since the reverse mapping is not serialized.
    fn from(serde: WatchersSerde) -> Self {
        let WatchersSerde {
            mapping,
            max_player_count,
        } = serde;
        let mut reverse_mapping: EnumMap<ValueKind, FxLinkedHashSet<Id>> = EnumMap::default();
        for (id, value) in &mapping {
            reverse_mapping[value.kind()].insert(*id);
        }
        Self {
            mapping,
            reverse_mapping,
            max_player_count,
        }
    }
}

/// Errors that can occur when managing watchers
#[derive(Error, Serialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The game has reached the maximum number of allowed players
    #[error("maximum number of players reached")]
    MaximumPlayers,
}

impl Watchers {
    /// Creates a new empty Watchers instance with the given maximum player count.
    pub fn new(max_player_count: usize) -> Self {
        Self {
            mapping: FxHashMap::default(),
            reverse_mapping: EnumMap::default(),
            max_player_count,
        }
    }

    /// Creates a new Watchers instance with a host already assigned.
    ///
    /// # Arguments
    ///
    /// * `host_id` - The ID of the host participant
    /// * `max_player_count` - The maximum number of players allowed
    pub fn with_host_id(host_id: Id, max_player_count: usize) -> Self {
        Self {
            mapping: {
                let mut map = FxHashMap::default();
                map.insert(host_id, Value::Host);
                map
            },
            reverse_mapping: {
                let mut map: EnumMap<ValueKind, FxLinkedHashSet<Id>> = EnumMap::default();
                map[ValueKind::Host].insert(host_id);
                map
            },
            max_player_count,
        }
    }

    /// Gets a vector of all participants with their tunnels and values
    ///
    /// # Arguments
    ///
    /// * `tunnel_finder` - Function to retrieve the tunnel for a given ID
    ///
    /// # Returns
    ///
    /// Vector of tuples containing (ID, Tunnel, Value) for all participants
    /// with active tunnels
    pub fn vec<F: TunnelFinder>(&self, tunnel_finder: F) -> Vec<(Id, F::Tunnel, Value)> {
        self.reverse_mapping
            .values()
            .flat_map(|v| v.iter())
            .filter_map(|x| match (tunnel_finder(*x), self.mapping.get(x)) {
                (Some(t), Some(v)) => Some((*x, t, v.to_owned())),
                _ => None,
            })
            .collect_vec()
    }

    /// Lazy iterator over `(Id, Tunnel, ValueKind)` for every live watcher.
    ///
    /// Like [`Self::vec`] but yields just the kind tag instead of cloning the
    /// full `Value`. Used by broadcast paths that branch on participant role
    /// without needing the inner data.
    pub fn iter_kinds<F: TunnelFinder>(&self, tunnel_finder: F) -> impl Iterator<Item = (Id, F::Tunnel, ValueKind)> {
        self.reverse_mapping
            .iter()
            .flat_map(move |(kind, ids)| ids.iter().map(move |id| (*id, kind)))
            .filter_map(move |(id, kind)| tunnel_finder(id).map(|t| (id, t, kind)))
    }

    /// Lazy iterator over participants of a specific type with active tunnels.
    ///
    /// Yields entries in insertion order; the iterator is double-ended so
    /// callers can `.rev()` for most-recent-first.
    pub fn specific_iter<F: TunnelFinder>(
        &self,
        filter: ValueKind,
        tunnel_finder: F,
    ) -> impl DoubleEndedIterator<Item = (Id, F::Tunnel, &Value)> {
        self.reverse_mapping[filter].iter().filter_map(move |id| {
            let tunnel = tunnel_finder(*id)?;
            let value = self.mapping.get(id)?;
            Some((*id, tunnel, value))
        })
    }

    /// Gets the count of participants of a specific type
    ///
    /// # Arguments
    ///
    /// * `filter` - The type of participants to count
    ///
    /// # Returns
    ///
    /// The number of participants of the specified type
    pub fn specific_count(&self, filter: ValueKind) -> usize {
        self.reverse_mapping[filter].len()
    }

    /// Adds a new watcher to the game session
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The unique ID for the new watcher
    /// * `watcher_value` - The value/role for the new watcher
    ///
    /// # Returns
    ///
    /// `Ok(())` if successful, or `Error::MaximumPlayers` if the game is full
    ///
    /// # Errors
    ///
    /// Returns `Error::MaximumPlayers` if adding this watcher would exceed
    /// the maximum allowed number of participants.
    pub fn add_watcher(&mut self, watcher_id: Id, watcher_value: Value) -> Result<(), Error> {
        let kind = watcher_value.kind();

        if self.mapping.len() >= self.max_player_count {
            return Err(Error::MaximumPlayers);
        }

        self.mapping.insert(watcher_id, watcher_value);
        self.reverse_mapping[kind].insert(watcher_id);

        Ok(())
    }

    /// Updates the value/role of an existing watcher
    ///
    /// This method properly handles moving the watcher between different
    /// type categories if their role changes. Always ensures the watcher is
    /// present in `reverse_mapping[new_kind]` (i.e. live) afterwards.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher to update
    /// * `watcher_value` - The new value/role for the watcher
    pub fn update_watcher_value(&mut self, watcher_id: Id, watcher_value: Value) {
        let old_kind = match self.mapping.get(&watcher_id) {
            Some(v) => v.kind(),
            _ => return,
        };
        let new_kind = watcher_value.kind();
        if old_kind != new_kind {
            self.reverse_mapping[old_kind].remove(&watcher_id);
        }
        self.reverse_mapping[new_kind].insert(watcher_id);
        self.mapping.insert(watcher_id, watcher_value);
    }

    /// Marks a watcher as no longer live (e.g. their session disconnected).
    ///
    /// Removes them from `reverse_mapping[kind]` so live-set queries like
    /// [`Self::specific_count`] and [`Self::specific_iter`] skip them, but
    /// keeps the entry in `mapping` so the watcher's role is preserved for
    /// reconnection. Idempotent.
    pub fn watcher_left(&mut self, watcher_id: Id) {
        if let Some(v) = self.mapping.get(&watcher_id) {
            self.reverse_mapping[v.kind()].remove(&watcher_id);
        }
    }

    /// Marks a watcher as live again (e.g. they reconnected). Idempotent.
    pub fn watcher_returned(&mut self, watcher_id: Id) {
        if let Some(v) = self.mapping.get(&watcher_id) {
            self.reverse_mapping[v.kind()].insert(watcher_id);
        }
    }

    /// Gets the value/role of a specific watcher
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher to look up
    ///
    /// # Returns
    ///
    /// The watcher's value if they exist, otherwise `None`
    pub fn get_watcher_value(&self, watcher_id: Id) -> Option<Value> {
        self.mapping.get(&watcher_id).map(std::borrow::ToOwned::to_owned)
    }

    /// Like [`Self::get_watcher_value`] but returns a reference instead of cloning.
    pub fn get_watcher_value_ref(&self, watcher_id: Id) -> Option<&Value> {
        self.mapping.get(&watcher_id)
    }

    /// Checks if a watcher exists in the game session
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher to check
    ///
    /// # Returns
    ///
    /// `true` if the watcher exists, `false` otherwise
    pub fn has_watcher(&self, watcher_id: Id) -> bool {
        self.mapping.contains_key(&watcher_id)
    }

    /// Checks if a watcher has an active connection
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher to check
    /// * `tunnel_finder` - Function to retrieve the tunnel for the watcher
    ///
    /// # Returns
    ///
    /// `true` if the watcher has an active tunnel, `false` otherwise
    pub fn is_alive<F: TunnelFinder>(watcher_id: Id, tunnel_finder: F) -> bool {
        tunnel_finder(watcher_id).is_some()
    }

    /// Removes a watcher's session and closes their tunnel
    ///
    /// This method finds the watcher's tunnel and properly closes it
    /// to clean up the connection.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher whose session should be removed
    /// * `tunnel_finder` - Function to retrieve the tunnel for the watcher
    pub fn remove_watcher_session<F: TunnelFinder>(watcher_id: Id, tunnel_finder: F) {
        Watchers::apply_to_session(watcher_id, tunnel_finder, Tunnel::close);
    }

    /// Sends an update message to a specific watcher
    ///
    /// # Arguments
    ///
    /// * `message` - The update message to send
    /// * `watcher_id` - The ID of the watcher to send to
    /// * `tunnel_finder` - Function to retrieve the tunnel for the watcher
    pub fn send_message<F: TunnelFinder>(message: &UpdateMessage, watcher_id: Id, tunnel_finder: F) {
        Watchers::apply_to_session(watcher_id, tunnel_finder, |session| {
            session.send_message(message);
        });
    }

    #[cfg_attr(coverage_nightly, coverage(off))]
    fn apply_to_session<F: TunnelFinder, A: FnOnce(F::Tunnel)>(watcher_id: Id, tunnel_finder: F, action: A) {
        if let Some(session) = tunnel_finder(watcher_id) {
            action(session);
        }
    }

    /// Sends a state synchronization message to a specific watcher
    ///
    /// # Arguments
    ///
    /// * `message` - The sync message to send
    /// * `watcher_id` - The ID of the watcher to send to
    /// * `tunnel_finder` - Function to retrieve the tunnel for the watcher
    pub fn send_state<F: TunnelFinder>(message: &SyncMessage, watcher_id: Id, tunnel_finder: F) {
        Watchers::apply_to_session(watcher_id, tunnel_finder, |session| {
            session.send_state(message);
        });
    }

    /// Gets the display name of a watcher
    ///
    /// This only returns a name for player watchers, not hosts or unassigned connections.
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher
    ///
    /// # Returns
    ///
    /// The player's name if they are a player, otherwise `None`
    pub fn get_name(&self, watcher_id: Id) -> Option<&str> {
        self.get_watcher_value_ref(watcher_id).and_then(|v| match v {
            Value::Player(player_value) => Some(player_value.name()),
            _ => None,
        })
    }

    /// Gets the team name of a watcher if they are part of a team
    ///
    /// # Arguments
    ///
    /// * `watcher_id` - The ID of the watcher
    ///
    /// # Returns
    ///
    /// The team name if the watcher is a team player, otherwise `None`
    pub fn get_team_name(&self, watcher_id: Id) -> Option<&str> {
        self.get_watcher_value_ref(watcher_id).and_then(|v| match v {
            Value::Player(PlayerValue::Team { team_name, .. }) => Some(team_name.as_str()),
            _ => None,
        })
    }

    /// Sends personalized messages to all watchers using a sender function
    ///
    /// The sender function is called for each watcher and can return different
    /// messages based on the watcher's ID and type, or None to skip sending.
    ///
    /// # Arguments
    ///
    /// * `sender` - Function that generates messages for each watcher
    /// * `tunnel_finder` - Function to retrieve tunnels for watchers
    pub fn announce_with<'a, S, F: TunnelFinder>(&self, sender: S, tunnel_finder: F)
    where
        S: Fn(Id, ValueKind) -> Option<super::UpdateMessage<'a>>,
    {
        for (watcher, session, kind) in self.iter_kinds(tunnel_finder) {
            let Some(message) = sender(watcher, kind) else {
                continue;
            };

            session.send_message(&message);
        }
    }

    /// Broadcasts an update message to all watchers except unassigned ones
    ///
    /// # Arguments
    ///
    /// * `message` - The update message to broadcast
    /// * `tunnel_finder` - Function to retrieve tunnels for watchers
    pub fn announce<F: TunnelFinder>(&self, message: &super::UpdateMessage<'_>, tunnel_finder: F) {
        self.announce_with(
            |_, value_kind| {
                if matches!(value_kind, ValueKind::Unassigned) {
                    None
                } else {
                    Some(message.to_owned())
                }
            },
            tunnel_finder,
        );
    }

    /// Sends an update message to all watchers of a specific type
    ///
    /// # Arguments
    ///
    /// * `filter` - The type of watchers to send to
    /// * `message` - The update message to send
    /// * `tunnel_finder` - Function to retrieve tunnels for watchers
    pub fn announce_specific<F: TunnelFinder>(
        &self,
        filter: ValueKind,
        message: &super::UpdateMessage<'_>,
        tunnel_finder: F,
    ) {
        for (_, session, _) in self.specific_iter(filter, tunnel_finder) {
            session.send_message(message);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone)]
    struct MockTunnel {
        // Stored as JSON so the tunnel doesn't need to thread the message
        // lifetime through its internal state — same shape as real tunnels.
        messages: Arc<Mutex<VecDeque<String>>>,
        states: Arc<Mutex<VecDeque<String>>>,
        closed: Arc<Mutex<bool>>,
    }

    impl MockTunnel {
        fn new() -> Self {
            Self {
                messages: Arc::new(Mutex::new(VecDeque::new())),
                states: Arc::new(Mutex::new(VecDeque::new())),
                closed: Arc::new(Mutex::new(false)),
            }
        }

        fn received_messages(&self) -> Vec<String> {
            self.messages.lock().unwrap().clone().into()
        }

        fn received_states(&self) -> Vec<String> {
            self.states.lock().unwrap().clone().into()
        }

        fn is_closed(&self) -> bool {
            *self.closed.lock().unwrap()
        }
    }

    impl Tunnel for MockTunnel {
        fn send_message(&self, message: &UpdateMessage<'_>) {
            self.messages
                .lock()
                .unwrap()
                .push_back(serde_json::to_string(message).unwrap_or_default());
        }

        fn send_state(&self, message: &SyncMessage<'_>) {
            self.states
                .lock()
                .unwrap()
                .push_back(serde_json::to_string(message).unwrap_or_default());
        }

        fn close(self) {
            *self.closed.lock().unwrap() = true;
        }
    }

    // Mock UpdateMessage for testing
    fn mock_update_message() -> UpdateMessage<'static> {
        UpdateMessage::Game(crate::game::UpdateMessage::IdAssign(Id::new()))
    }

    // Mock SyncMessage for testing
    fn mock_sync_message() -> SyncMessage<'static> {
        SyncMessage::Game(crate::game::SyncMessage::WaitingScreen(crate::TruncatedVec::default()))
    }

    #[test]
    fn test_id_creation_and_formatting() {
        let id = Id::new();
        let id_string = id.to_string();

        // Should be a valid UUID string
        assert_eq!(id_string.len(), 36); // UUID format: xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx
        assert!(id_string.contains('-'));

        // Should be parseable back to ID
        let parsed_id = Id::from_str(&id_string).unwrap();
        assert_eq!(id, parsed_id);
    }

    #[test]
    fn test_id_default() {
        let id1 = Id::default();
        let id2 = Id::default();

        // Default should create different IDs
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_id_from_invalid_string() {
        let result = Id::from_str("invalid-uuid");
        assert!(result.is_err());
    }

    #[test]
    fn test_value_kind() {
        assert_eq!(Value::Unassigned.kind(), ValueKind::Unassigned);
        assert_eq!(Value::Host.kind(), ValueKind::Host);

        let player_value = PlayerValue::Individual {
            name: "Alice".to_string(),
        };
        assert_eq!(Value::Player(player_value).kind(), ValueKind::Player);
    }

    #[test]
    fn test_player_value_name() {
        let individual = PlayerValue::Individual {
            name: "Alice".to_string(),
        };
        assert_eq!(individual.name(), "Alice");

        let team_player = PlayerValue::Team {
            team_name: "Team A".to_string(),
            individual_name: "Bob".to_string(),
            team_id: Id::new(),
        };
        assert_eq!(team_player.name(), "Bob");
    }

    #[test]
    fn test_watchers_default() {
        let watchers = Watchers::new(1000);
        assert_eq!(watchers.specific_count(ValueKind::Unassigned), 0);
        assert_eq!(watchers.specific_count(ValueKind::Host), 0);
        assert_eq!(watchers.specific_count(ValueKind::Player), 0);
    }

    #[test]
    fn test_watchers_with_host_id() {
        let host_id = Id::new();
        let watchers = Watchers::with_host_id(host_id, 1000);

        assert_eq!(watchers.specific_count(ValueKind::Host), 1);
        assert_eq!(watchers.specific_count(ValueKind::Player), 0);
        assert_eq!(watchers.specific_count(ValueKind::Unassigned), 0);
        assert!(watchers.has_watcher(host_id));
        assert_eq!(watchers.get_watcher_value(host_id), Some(Value::Host));
    }

    #[test]
    fn test_add_watcher() {
        let mut watchers = Watchers::new(1000);
        let watcher_id = Id::new();

        let result = watchers.add_watcher(watcher_id, Value::Unassigned);
        assert!(result.is_ok());

        assert!(watchers.has_watcher(watcher_id));
        assert_eq!(watchers.specific_count(ValueKind::Unassigned), 1);
        assert_eq!(watchers.get_watcher_value(watcher_id), Some(Value::Unassigned));
    }

    #[test]
    fn test_add_player_watcher() {
        let mut watchers = Watchers::new(1000);
        let player_id = Id::new();
        let player_value = Value::Player(PlayerValue::Individual {
            name: "Alice".to_string(),
        });

        let result = watchers.add_watcher(player_id, player_value.clone());
        assert!(result.is_ok());

        assert!(watchers.has_watcher(player_id));
        assert_eq!(watchers.specific_count(ValueKind::Player), 1);
        assert_eq!(watchers.get_watcher_value(player_id), Some(player_value));
        assert_eq!(watchers.get_name(player_id), Some("Alice"));
    }

    #[test]
    fn test_add_team_player_watcher() {
        let mut watchers = Watchers::new(1000);
        let player_id = Id::new();
        let team_id = Id::new();
        let player_value = Value::Player(PlayerValue::Team {
            team_name: "Team A".to_string(),
            individual_name: "Bob".to_string(),
            team_id,
        });

        let result = watchers.add_watcher(player_id, player_value.clone());
        assert!(result.is_ok());

        assert!(watchers.has_watcher(player_id));
        assert_eq!(watchers.specific_count(ValueKind::Player), 1);
        assert_eq!(watchers.get_name(player_id), Some("Bob"));
        assert_eq!(watchers.get_team_name(player_id), Some("Team A"));
    }

    #[test]
    fn test_maximum_players_error() {
        let mut watchers = Watchers::new(1000);

        // Add players up to the maximum
        for i in 0..crate::settings::FuizSettings::default().max_player_count {
            let watcher_id = Id::new();
            let result = watchers.add_watcher(watcher_id, Value::Unassigned);
            assert!(result.is_ok(), "Failed to add player {i}");
        }

        // Adding one more should fail
        let extra_watcher_id = Id::new();
        let result = watchers.add_watcher(extra_watcher_id, Value::Unassigned);
        assert_eq!(result.err(), Some(Error::MaximumPlayers));
    }

    #[test]
    fn test_update_watcher_value() {
        let mut watchers = Watchers::new(1000);
        let watcher_id = Id::new();

        // Start as unassigned
        watchers.add_watcher(watcher_id, Value::Unassigned).unwrap();
        assert_eq!(watchers.specific_count(ValueKind::Unassigned), 1);
        assert_eq!(watchers.specific_count(ValueKind::Player), 0);

        // Update to player
        let player_value = Value::Player(PlayerValue::Individual {
            name: "Alice".to_string(),
        });
        watchers.update_watcher_value(watcher_id, player_value.clone());

        assert_eq!(watchers.specific_count(ValueKind::Unassigned), 0);
        assert_eq!(watchers.specific_count(ValueKind::Player), 1);
        assert_eq!(watchers.get_watcher_value(watcher_id), Some(player_value));
    }

    #[test]
    fn test_update_nonexistent_watcher() {
        let mut watchers = Watchers::new(1000);
        let nonexistent_id = Id::new();

        // This should not panic and should be a no-op
        watchers.update_watcher_value(nonexistent_id, Value::Host);
        assert!(!watchers.has_watcher(nonexistent_id));
    }

    #[test]
    fn test_vec_with_tunnels() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        // Add some watchers
        let id1 = Id::new();
        let id2 = Id::new();
        let id3 = Id::new();

        watchers.add_watcher(id1, Value::Host).unwrap();
        watchers
            .add_watcher(
                id2,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();
        watchers.add_watcher(id3, Value::Unassigned).unwrap();

        // Add tunnels for some watchers
        tunnels.insert(id1, MockTunnel::new());
        tunnels.insert(id2, MockTunnel::new());
        // id3 has no tunnel

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();
        let vec_result = watchers.vec(tunnel_finder);

        // Should only include watchers with tunnels
        assert_eq!(vec_result.len(), 2);

        let ids: HashSet<Id> = vec_result.iter().map(|(id, _, _)| *id).collect();
        assert!(ids.contains(&id1));
        assert!(ids.contains(&id2));
        assert!(!ids.contains(&id3));
    }

    #[test]
    fn test_specific_iter() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let host_id = Id::new();
        let player1_id = Id::new();
        let player2_id = Id::new();

        watchers.add_watcher(host_id, Value::Host).unwrap();
        watchers
            .add_watcher(
                player1_id,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();
        watchers
            .add_watcher(
                player2_id,
                Value::Player(PlayerValue::Individual {
                    name: "Bob".to_string(),
                }),
            )
            .unwrap();

        // Tunnel for host + player1, not for player2 — so player2 is filtered.
        tunnels.insert(host_id, MockTunnel::new());
        tunnels.insert(player1_id, MockTunnel::new());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();

        let players: Vec<_> = watchers
            .specific_iter(ValueKind::Player, tunnel_finder)
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(players, vec![player1_id]);

        let hosts: Vec<_> = watchers
            .specific_iter(ValueKind::Host, tunnel_finder)
            .map(|(id, _, _)| id)
            .collect();
        assert_eq!(hosts, vec![host_id]);
    }

    #[test]
    fn test_is_alive() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let id1 = Id::new();
        let id2 = Id::new();

        watchers.add_watcher(id1, Value::Host).unwrap();
        watchers.add_watcher(id2, Value::Unassigned).unwrap();

        // Only add tunnel for id1
        tunnels.insert(id1, MockTunnel::new());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();

        assert!(Watchers::is_alive(id1, tunnel_finder));
        assert!(!Watchers::is_alive(id2, tunnel_finder));
    }

    #[test]
    fn test_remove_watcher_session() {
        let mut tunnels = HashMap::new();

        let id = Id::new();

        let tunnel = MockTunnel::new();
        tunnels.insert(id, tunnel.clone());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();

        assert!(!tunnel.is_closed());
        Watchers::remove_watcher_session(id, tunnel_finder);
        assert!(tunnel.is_closed());
    }

    #[test]
    fn test_remove_watcher_session_no_tunnel() {
        // Tunnel finder that returns None (no tunnel available)
        let id = Id::new();

        let tunnel_finder = |_id: Id| -> Option<MockTunnel> { None };

        // Should not panic or error when tunnel_finder returns None
        Watchers::remove_watcher_session(id, tunnel_finder);
    }

    #[test]
    fn test_send_message() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let id = Id::new();
        watchers.add_watcher(id, Value::Host).unwrap();

        let tunnel = MockTunnel::new();
        tunnels.insert(id, tunnel.clone());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();
        let message = mock_update_message();

        Watchers::send_message(&message, id, tunnel_finder);

        let received = tunnel.received_messages();
        assert_eq!(received.len(), 1);
    }

    #[test]
    fn test_send_message_no_tunnel() {
        let mut watchers = Watchers::new(1000);
        let watcher_id = Id::new();

        watchers.add_watcher(watcher_id, Value::Host).unwrap();

        // Tunnel finder that returns None (no tunnel available)
        let tunnel_finder = |_id: Id| -> Option<MockTunnel> { None };
        let message = mock_update_message();

        // This should not panic and should be a no-op
        Watchers::send_message(&message, watcher_id, tunnel_finder);

        // Test passes if no panic occurs
    }

    #[test]
    fn test_send_state() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let id = Id::new();
        watchers.add_watcher(id, Value::Host).unwrap();

        let tunnel = MockTunnel::new();
        tunnels.insert(id, tunnel.clone());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();
        let message = mock_sync_message();

        Watchers::send_state(&message, id, tunnel_finder);

        let received = tunnel.received_states();
        assert_eq!(received.len(), 1);
    }

    #[test]
    fn test_send_state_no_tunnel() {
        let mut watchers = Watchers::new(1000);
        let watcher_id = Id::new();

        watchers.add_watcher(watcher_id, Value::Host).unwrap();

        // Tunnel finder that returns None (no tunnel available)
        let tunnel_finder = |_id: Id| -> Option<MockTunnel> { None };
        let message = mock_sync_message();

        // This should not panic and should be a no-op
        Watchers::send_state(&message, watcher_id, tunnel_finder);

        // Test passes if no panic occurs
    }

    #[test]
    fn test_get_name_for_non_player() {
        let mut watchers = Watchers::new(1000);
        let host_id = Id::new();
        let unassigned_id = Id::new();

        watchers.add_watcher(host_id, Value::Host).unwrap();
        watchers.add_watcher(unassigned_id, Value::Unassigned).unwrap();

        assert_eq!(watchers.get_name(host_id), None);
        assert_eq!(watchers.get_name(unassigned_id), None);
    }

    #[test]
    fn test_get_team_name_for_non_team_player() {
        let mut watchers = Watchers::new(1000);
        let individual_id = Id::new();
        let host_id = Id::new();

        watchers
            .add_watcher(
                individual_id,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();
        watchers.add_watcher(host_id, Value::Host).unwrap();

        assert_eq!(watchers.get_team_name(individual_id), None);
        assert_eq!(watchers.get_team_name(host_id), None);
    }

    #[test]
    fn test_announce() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let host_id = Id::new();
        let player_id = Id::new();
        let unassigned_id = Id::new();

        watchers.add_watcher(host_id, Value::Host).unwrap();
        watchers
            .add_watcher(
                player_id,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();
        watchers.add_watcher(unassigned_id, Value::Unassigned).unwrap();

        let host_tunnel = MockTunnel::new();
        let player_tunnel = MockTunnel::new();
        let unassigned_tunnel = MockTunnel::new();

        tunnels.insert(host_id, host_tunnel.clone());
        tunnels.insert(player_id, player_tunnel.clone());
        tunnels.insert(unassigned_id, unassigned_tunnel.clone());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();
        let message = mock_update_message();

        watchers.announce(&message, tunnel_finder);

        // Host and player should receive the message
        assert_eq!(host_tunnel.received_messages().len(), 1);
        assert_eq!(player_tunnel.received_messages().len(), 1);

        // Unassigned should not receive the message
        assert_eq!(unassigned_tunnel.received_messages().len(), 0);
    }

    #[test]
    fn test_announce_specific() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let host_id = Id::new();
        let player_id = Id::new();

        watchers.add_watcher(host_id, Value::Host).unwrap();
        watchers
            .add_watcher(
                player_id,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();

        let host_tunnel = MockTunnel::new();
        let player_tunnel = MockTunnel::new();

        tunnels.insert(host_id, host_tunnel.clone());
        tunnels.insert(player_id, player_tunnel.clone());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();
        let message = mock_update_message();

        // Send only to players
        watchers.announce_specific(ValueKind::Player, &message, tunnel_finder);

        // Only player should receive the message
        assert_eq!(host_tunnel.received_messages().len(), 0);
        assert_eq!(player_tunnel.received_messages().len(), 1);
    }

    #[test]
    fn test_announce_with() {
        let mut watchers = Watchers::new(1000);
        let mut tunnels = HashMap::new();

        let host_id = Id::new();
        let player_id = Id::new();

        watchers.add_watcher(host_id, Value::Host).unwrap();
        watchers
            .add_watcher(
                player_id,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();

        let host_tunnel = MockTunnel::new();
        let player_tunnel = MockTunnel::new();

        tunnels.insert(host_id, host_tunnel.clone());
        tunnels.insert(player_id, player_tunnel.clone());

        let tunnel_finder = |id: Id| tunnels.get(&id).cloned();

        // Custom sender that only sends to hosts
        let sender = |_id: Id, kind: ValueKind| {
            if matches!(kind, ValueKind::Host) {
                Some(mock_update_message())
            } else {
                None
            }
        };

        watchers.announce_with(sender, tunnel_finder);

        // Only host should receive the message
        assert_eq!(host_tunnel.received_messages().len(), 1);
        assert_eq!(player_tunnel.received_messages().len(), 0);
    }

    #[cfg(feature = "serializable")]
    #[test]
    fn test_serde_roundtrip() {
        let mut watchers = Watchers::new(1000);
        let host_id = Id::new();
        let player_id = Id::new();

        watchers.add_watcher(host_id, Value::Host).unwrap();
        watchers
            .add_watcher(
                player_id,
                Value::Player(PlayerValue::Individual {
                    name: "Alice".to_string(),
                }),
            )
            .unwrap();

        // Serialize
        let json = serde_json::to_string(&watchers).unwrap();

        // Deserialize
        let deserialized: Watchers = serde_json::from_str(&json).unwrap();

        // Check that the reverse mapping was properly reconstructed
        assert_eq!(deserialized.specific_count(ValueKind::Host), 1);
        assert_eq!(deserialized.specific_count(ValueKind::Player), 1);
        assert!(deserialized.has_watcher(host_id));
        assert!(deserialized.has_watcher(player_id));
        assert_eq!(deserialized.get_watcher_value(host_id), Some(Value::Host));
    }

    #[test]
    fn test_error_display() {
        let error = Error::MaximumPlayers;
        assert_eq!(error.to_string(), "maximum number of players reached");
    }

    #[test]
    fn test_id_serialize_deserialize() {
        let id = Id::new();

        // Test serialization to JSON (using SerializeDisplay)
        let serialized = serde_json::to_string(&id).unwrap();
        // Should be a quoted UUID string
        assert!(serialized.starts_with('"'));
        assert!(serialized.ends_with('"'));
        assert_eq!(serialized.len(), 38); // 36 chars + 2 quotes

        // Test deserialization from JSON
        let deserialized: Id = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);

        // Test round-trip consistency
        let id_string = id.to_string();
        let parsed_id: Id = serde_json::from_str(&format!("\"{id_string}\"")).unwrap();
        assert_eq!(id, parsed_id);
    }
}
