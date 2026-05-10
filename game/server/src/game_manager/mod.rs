use std::time::Duration;

use enum_map::EnumMap;
use thiserror::Error;

use crate::{Session, clashmap::ClashMap};

use fuiz::{
    AlarmMessage,
    fuiz::config::Fuiz,
    game::{self, Game, IncomingMessage, Options},
    game_id::GameId,
    watcher::{self, Id},
};

#[derive(Debug, Default)]
struct SharedGame(parking_lot::RwLock<Option<Box<Game>>>);

impl SharedGame {
    pub fn with_game<R>(&self, f: impl FnOnce(&Game) -> R) -> Option<R> {
        let guard = self.0.read();
        let game = guard.as_deref()?;
        if matches!(game.state, game::State::Done) {
            None
        } else {
            Some(f(game))
        }
    }

    pub fn with_game_mut<R>(&self, f: impl FnOnce(&mut Game) -> R) -> Option<R> {
        let mut guard = self.0.write();
        let game = guard.as_deref_mut()?;
        if matches!(game.state, game::State::Done) {
            None
        } else {
            Some(f(game))
        }
    }

    pub fn with_game_raw<R>(&self, f: impl FnOnce(&Game) -> R) -> Option<R> {
        let guard = self.0.read();
        let game = guard.as_deref()?;
        Some(f(game))
    }
}

#[derive(Default)]
pub struct GameManager {
    games: EnumMap<GameId, SharedGame>,
    watcher_mapping: ClashMap<Id, Session>,
}

#[derive(Debug, Error)]
#[error("game does not exist")]
pub struct GameVanish {}

impl actix_web::error::ResponseError for GameVanish {
    fn status_code(&self) -> actix_web::http::StatusCode {
        actix_web::http::StatusCode::NOT_FOUND
    }
}

impl GameManager {
    fn with_game<R>(&self, game_id: GameId, f: impl FnOnce(&Game) -> R) -> Result<R, GameVanish> {
        self.games[game_id].with_game(f).ok_or(GameVanish {})
    }

    fn with_game_mut<R>(&self, game_id: GameId, f: impl FnOnce(&mut Game) -> R) -> Result<R, GameVanish> {
        self.games[game_id].with_game_mut(f).ok_or(GameVanish {})
    }

    pub fn add_game(&self, fuiz: Fuiz, options: Options, host_id: Id, settings: &fuiz::settings::Settings) -> GameId {
        let shared_game = Box::new(Game::new(fuiz, options, host_id, settings));

        loop {
            let game_id = GameId::new();

            let Some(mut game) = self.games[game_id].0.try_write() else {
                continue;
            };

            if game.is_none() {
                *game = Some(shared_game);
                return game_id;
            }
        }
    }

    fn tunnel_finder(&self, watcher_id: Id) -> Option<Session> {
        self.watcher_mapping.get(&watcher_id)
    }

    pub fn set_tunnel(&self, watcher_id: Id, tunnel: Session) -> Option<Session> {
        self.watcher_mapping.insert(watcher_id, tunnel)
    }

    pub fn remove_tunnel(&self, watcher_id: Id) -> Option<Session> {
        self.watcher_mapping.remove(&watcher_id).map(|(_, s)| s)
    }

    pub fn add_unassigned(&self, game_id: GameId, watcher_id: Id) -> Result<Result<(), watcher::Error>, GameVanish> {
        self.with_game_mut(game_id, |game| {
            game.add_unassigned(watcher_id, |id| self.tunnel_finder(id))
        })
    }

    pub fn is_game_done(&self, game_id: GameId) -> Result<bool, GameVanish> {
        self.games[game_id]
            .with_game_raw(|game| matches!(game.state, game::State::Done))
            .ok_or(GameVanish {})
    }

    pub fn watcher_exists(&self, game_id: GameId, watcher_id: Id) -> Result<bool, GameVanish> {
        self.with_game(game_id, |game| game.watchers.has_watcher(watcher_id))
    }

    pub fn receive_message<F: Fn(AlarmMessage, Duration)>(
        &self,
        game_id: GameId,
        watcher_id: Id,
        message: IncomingMessage,
        schedule_message: F,
    ) -> Result<(), GameVanish> {
        self.with_game_mut(game_id, |game| {
            game.receive_message(watcher_id, message, schedule_message, |id| self.tunnel_finder(id));
        })
    }

    pub fn receive_alarm<F: Fn(AlarmMessage, Duration)>(
        &self,
        game_id: GameId,
        alarm_message: &AlarmMessage,
        schedule_message: F,
    ) -> Result<(), GameVanish> {
        self.with_game_mut(game_id, |game| {
            game.receive_alarm(alarm_message, schedule_message, |id| self.tunnel_finder(id));
        })
    }

    pub fn watcher_left(&self, game_id: GameId, watcher_id: Id) -> Result<(), GameVanish> {
        self.with_game_mut(game_id, |game| {
            game.watcher_left(watcher_id, |id| self.tunnel_finder(id));
        })
    }

    pub fn exists(&self, game_id: GameId) -> Result<(), GameVanish> {
        self.with_game(game_id, |_| ())
    }

    pub fn update_session(&self, game_id: GameId, watcher_id: Id) -> Result<(), GameVanish> {
        self.with_game_mut(game_id, |game| {
            game.update_session(watcher_id, |id| self.tunnel_finder(id));
        })
    }

    pub fn remove_game(&self, game_id: GameId) {
        let mut game = self.games[game_id].0.write();
        if let Some(mut ongoing_game) = game.take() {
            ongoing_game.mark_as_done(|id| self.tunnel_finder(id));
        }
    }
}
