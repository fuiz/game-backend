use std::{
    cell::{RefCell, RefMut},
    str::FromStr,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use worker::*;

use fuiz::{
    game,
    session::Tunnel,
    watcher::{self},
};

#[derive(Debug, serde::Deserialize, garde::Validate, Serialize)]
#[garde(context(fuiz::settings::Settings))]
pub struct GameRequest {
    #[garde(dive)]
    pub config: fuiz::fuiz::config::Fuiz,
    #[garde(dive)]
    pub options: fuiz::game::Options,
}

struct WebSocketTunnel(WebSocket);

impl Tunnel for WebSocketTunnel {
    fn close(self) {
        let _ = self.0.close::<String>(None, None);
    }

    fn send_message(&self, message: &fuiz::UpdateMessage) {
        let message = serde_json::to_string(message).expect("Failed to serialize message");

        let _ = self.0.send_with_str(message);
    }

    fn send_state(&self, state: &fuiz::SyncMessage) {
        let message = serde_json::to_string(state).expect("Failed to serialize state");

        let _ = self.0.send_with_str(message);
    }
}

#[durable_object]
pub struct Game {
    game: RefCell<Option<fuiz::game::Game>>,
    alarm_message: RefCell<Option<AlarmMessage>>,
    state: State,
    env: Env,
}

#[derive(Serialize, Deserialize)]
enum AlarmMessage {
    DeleteGame,
    Game(fuiz::AlarmMessage),
}

impl Game {
    async fn load_state(&self) {
        if self.game.borrow().is_none() {
            if let Some(game) = load_game(&self.state.storage()).await {
                self.game.replace(Some(game));
            } else {
                self.game.replace(None);
            }
            self.alarm_message
                .replace(self.state.storage().get("alarm").await.ok().flatten());
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(transparent)]
struct GameBytes {
    #[serde(with = "serde_bytes")]
    bytes: Vec<u8>,
}

async fn load_game(storage: &worker::durable::Storage) -> Option<fuiz::game::Game> {
    let count = storage.get("count").await.ok()??;

    let mut game_bytes = Vec::new();

    for i in 0..count {
        let array_buffer: Result<Option<GameBytes>> = storage.get(&format!("chunk_{i}")).await;
        match array_buffer {
            Err(e) => {
                console_error!("Error loading chunk: {:?}", e);
                return None;
            }
            Ok(None) => {
                console_error!("Chunk {} not found", i);
                return None;
            }
            Ok(Some(string_chunk)) => {
                game_bytes.extend_from_slice(&string_chunk.bytes);
            }
        }
    }

    let game = ciborium::from_reader(game_bytes.as_slice());

    match game {
        Ok(game) => Some(game),
        Err(e) => {
            console_error!("Error deserializing game: {:?}", e);
            None
        }
    }
}

fn get_serialized_game(game: &fuiz::game::Game) -> Result<Vec<u8>> {
    let mut game_bytes = Vec::new();

    ciborium::into_writer(game, &mut game_bytes).map_err(|e| {
        console_error!("Error serializing game: {:?}", e);
        worker::Error::RustError(e.to_string())
    })?;

    Ok(game_bytes)
}

async fn store_game(storage: &mut worker::durable::Storage, game_bytes: &[u8]) -> Result<()> {
    let chunks_of_64kb = game_bytes
        .chunks(64 * 1024)
        .map(|chunk| GameBytes { bytes: chunk.to_vec() })
        .collect::<Vec<_>>();

    storage.put("count", &chunks_of_64kb.len()).await?;

    for (i, chunk) in chunks_of_64kb.into_iter().enumerate() {
        if let Err(e) = storage.put(&format!("chunk_{i}"), &chunk).await {
            console_error!("Error storing chunk: {:?}", e);
        }
    }
    Ok(())
}

const GAME_EXPIRY: Duration = Duration::from_hours(1);

impl Game {
    fn borrow_game_mut(&self) -> Option<RefMut<'_, fuiz::game::Game>> {
        let game = RefMut::filter_map(self.game.borrow_mut(), std::option::Option::as_mut);

        game.ok()
    }

    fn borrow_game(&self) -> Option<std::cell::Ref<'_, fuiz::game::Game>> {
        let game = std::cell::Ref::filter_map(self.game.borrow(), std::option::Option::as_ref);

        game.ok()
    }

    fn with_mut_game<F, R>(&self, f: F) -> Option<R>
    where
        F: FnOnce(&mut fuiz::game::Game) -> R,
    {
        self.borrow_game_mut().map(|mut game| f(&mut game))
    }

    async fn with_mut_game_update_storage<F, R>(&self, f: F) -> Result<Option<R>>
    where
        F: FnOnce(&mut fuiz::game::Game) -> R,
    {
        let Some((ret, game_bytes)) = self.with_mut_game(|game| {
            let ret = f(game);
            (ret, get_serialized_game(game))
        }) else {
            return Ok(None);
        };

        store_game(&mut self.state.storage(), &game_bytes?).await?;

        Ok(Some(ret))
    }

    async fn with_mut_game_alarm_message_update_storage<F>(&self, f: F) -> Result<()>
    where
        F: FnOnce(&mut fuiz::game::Game) -> Option<(fuiz::AlarmMessage, Duration)>,
    {
        let Some((alarm_message_duration, game_bytes)) = self.with_mut_game(|game| {
            let alarm_message_duration = f(game);

            (alarm_message_duration, get_serialized_game(game))
        }) else {
            return Ok(());
        };

        store_game(&mut self.state.storage(), &game_bytes?).await?;

        if let Some((message, duration)) = alarm_message_duration {
            self.alarm_message.replace(Some(AlarmMessage::Game(message)));
            self.state.storage().set_alarm(duration).await?;
        } else if self.state.storage().get_alarm().await.unwrap().is_none() {
            self.alarm_message.replace(Some(AlarmMessage::DeleteGame));
            self.state.storage().set_alarm(GAME_EXPIRY).await?;
        }

        self.state.storage().put("alarm", &self.alarm_message).await?;

        Ok(())
    }

    fn tunnel_finder(&self) -> impl Fn(watcher::Id) -> Option<WebSocketTunnel> + '_ {
        |id| {
            self.state
                .get_websockets_with_tag(&id.to_string())
                .first()
                .map(|ws| WebSocketTunnel(ws.to_owned()))
        }
    }

    async fn increment_player_count(&self) -> Result<()> {
        self.env
            .service("COUNTER")?
            .fetch("https://example.com/player_count", {
                Some(RequestInit {
                    method: Method::Post,
                    ..RequestInit::default()
                })
            })
            .await?;

        Ok(())
    }
}

impl DurableObject for Game {
    fn new(state: State, env: Env) -> Self {
        Self {
            game: None.into(),
            alarm_message: None.into(),
            state,
            env,
        }
    }

    async fn alarm(&self) -> Result<Response> {
        self.load_state().await;

        let alarm_message_to_be_announced = self.alarm_message.take();

        match alarm_message_to_be_announced {
            Some(AlarmMessage::DeleteGame) => {
                self.state.storage().delete_all().await?;
                return Response::ok("");
            }
            Some(AlarmMessage::Game(message)) => {
                self.with_mut_game_alarm_message_update_storage(|game| {
                    let mut alarm_message_duration = None;

                    let schedule_message = |message: fuiz::AlarmMessage, duration: Duration| {
                        alarm_message_duration = Some((message, duration));
                    };

                    game.receive_alarm(&message, schedule_message, self.tunnel_finder());

                    alarm_message_duration
                })
                .await?;
            }
            _ => {}
        }

        Response::ok("")
    }

    async fn fetch(&self, mut req: Request) -> Result<Response> {
        self.load_state().await;

        if req.url()?.path().starts_with("/add") {
            let game_request = req.json::<GameRequest>().await?;

            let host_id = watcher::Id::new();

            let settings = fuiz::settings::Settings::default();
            self.game.replace(Some(fuiz::game::Game::new(
                game_request.config,
                game_request.options,
                host_id,
                &settings,
            )));
            return Response::ok(host_id.to_string());
        }

        if req.url()?.path().starts_with("/alive") {
            let Some(game) = self.borrow_game() else {
                return Response::ok("false");
            };

            return Response::ok(if matches!(game.state, game::State::Done) {
                "false"
            } else {
                "true"
            });
        }

        let WebSocketPair { client, server } = WebSocketPair::new()?;

        let claimed_id = req
            .url()?
            .path_segments()
            .and_then(|mut ps| ps.next_back())
            .and_then(|s| watcher::Id::from_str(s).clone().ok())
            .unwrap_or(watcher::Id::new());

        close_connections_with_tag(&self.state, &claimed_id);
        self.state
            .accept_websocket_with_tags(&server, &[&claimed_id.to_string()]);
        server.serialize_attachment(claimed_id)?;

        Response::from_websocket(client)
    }

    async fn websocket_message(&self, ws: WebSocket, message: WebSocketIncomingMessage) -> Result<()> {
        self.load_state().await;

        {
            let WebSocketIncomingMessage::String(serialized_message) = message else {
                return Ok(());
            };

            let Ok(message) = serde_json::from_str(serialized_message.as_ref()) else {
                return Ok(());
            };

            let watcher_id = ws.deserialize_attachment::<watcher::Id>()?;

            if let Some(watcher_id) = watcher_id {
                match message {
                    game::IncomingMessage::Ghost(game::IncomingGhostMessage::DemandId) => {
                        close_connections_with_tag_except_one(&self.state, &watcher_id, &ws);
                        let session = WebSocketTunnel(ws);

                        session.send_message(&game::UpdateMessage::IdAssign(watcher_id).into());

                        self.with_mut_game_update_storage(|game| {
                            if game.add_unassigned(watcher_id, self.tunnel_finder()).is_err() {
                                session.close();
                            }
                        })
                        .await?;

                        if let Err(e) = self.increment_player_count().await {
                            console_error!("Error incrementing player count: {:?}", e);
                        }
                    }
                    game::IncomingMessage::Ghost(_) => {
                        close_connections_with_tag_except_one(&self.state, &watcher_id, &ws);

                        let session = WebSocketTunnel(ws);

                        session.send_message(&game::UpdateMessage::IdAssign(watcher_id).into());

                        self.with_mut_game_update_storage(|game| {
                            game.update_session(watcher_id, self.tunnel_finder());
                        })
                        .await?;
                    }
                    message => {
                        self.with_mut_game_alarm_message_update_storage(|game| {
                            let mut alarm_message_duration = None;

                            let schedule_message = |message: fuiz::AlarmMessage, duration: Duration| {
                                alarm_message_duration = Some((message, duration));
                            };

                            game.receive_message(watcher_id, message, schedule_message, self.tunnel_finder());

                            alarm_message_duration
                        })
                        .await?;
                    }
                }
            } else {
                let game::IncomingMessage::Ghost(ghost_message) = message else {
                    return Ok(());
                };

                self.with_mut_game_update_storage(|game| {
                    if let game::IncomingGhostMessage::ClaimId(id) = ghost_message
                        && game.watchers.has_watcher(id)
                    {
                        close_connections_with_tag(&self.state, &id);
                        ws.serialize_attachment(id)?;

                        game.update_session(id, self.tunnel_finder());
                    } else {
                        let new_id = watcher::Id::new();

                        ws.serialize_attachment(new_id)?;

                        let session = WebSocketTunnel(ws);

                        session.send_message(&game::UpdateMessage::IdAssign(new_id).into());

                        if game.add_unassigned(new_id, self.tunnel_finder()).is_err() {
                            session.close();
                        }
                    }

                    Ok::<(), worker::Error>(())
                })
                .await?
                .transpose()?;
            }
        }

        Ok(())
    }

    async fn websocket_close(&self, ws: WebSocket, _code: usize, _reason: String, _was_clean: bool) -> Result<()> {
        let Some(watcher_id) = ws.deserialize_attachment::<watcher::Id>()? else {
            return Ok(());
        };

        self.load_state().await;

        self.with_mut_game_update_storage(|game| {
            game.watcher_left(watcher_id);
        })
        .await?;

        Ok(())
    }
}

fn close_connections_with_tag_except_one(state: &State, tag: &watcher::Id, ws: &WebSocket) {
    state
        .get_websockets_with_tag(&tag.to_string())
        .into_iter()
        .filter(|web_socket| web_socket != ws)
        .for_each(close_web_socket);
}

fn close_connections_with_tag(state: &State, tag: &watcher::Id) {
    state
        .get_websockets_with_tag(&tag.to_string())
        .into_iter()
        .for_each(close_web_socket);
}

#[allow(clippy::needless_pass_by_value)]
fn close_web_socket(web_socket: WebSocket) {
    let _ = web_socket.close(Some(4141), None::<String>);
}
