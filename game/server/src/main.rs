//! Fuiz game server.

mod clashmap;
mod game_manager;

use actix_web::{
    App, HttpRequest, HttpResponse, HttpServer, Responder, get,
    middleware::Logger,
    post,
    web::{self, Data},
};
use figment::{
    Figment,
    providers::{Env, Serialized},
};
use fuiz::game::{IncomingGhostMessage, Options};
use fuiz::{
    fuiz::config::Fuiz,
    game::{IncomingMessage, UpdateMessage},
    game_id::GameId,
    session::Tunnel,
    watcher::Id,
};
use futures_util::StreamExt;
use game_manager::GameManager;
use garde::Validate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

extern crate pretty_env_logger;
#[macro_use]
extern crate log;

/// Server configuration loaded from environment variables with sensible defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerConfig {
    /// Hostname to bind to.
    hostname: String,
    /// Port to bind to.
    port: u16,
    /// Allowed CORS origins. Empty means permissive.
    allowed_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            hostname: "0.0.0.0".into(),
            port: 8080,
            allowed_origins: Vec::new(),
        }
    }
}

/// A WebSocket session wrapper.
#[derive(Clone)]
pub struct Session {
    session: actix_ws::Session,
}

impl Session {
    /// Creates a new session from an actix WebSocket session.
    pub fn new(session: actix_ws::Session) -> Self {
        Self { session }
    }
}

type MessageScheduler = Box<dyn Fn(fuiz::AlarmMessage, Duration)>;

impl fuiz::session::Tunnel for Session {
    fn send_message(&self, message: &fuiz::UpdateMessage) {
        let mut session = self.session.clone();

        let message = serde_json::to_string(message).expect("message should be serializable");

        actix_web::rt::spawn(async move {
            let _ = session.text(message).await;
        });
    }

    fn send_state(&self, state: &fuiz::SyncMessage) {
        let mut session = self.session.clone();

        let message = serde_json::to_string(state).expect("message should be serializable");

        actix_web::rt::spawn(async move {
            let _ = session.text(message).await;
        });
    }

    fn close(self) {
        actix_web::rt::spawn(async move {
            let _ = self.session.close(None).await;
        });
    }
}

struct AppState {
    game_manager: GameManager,
    settings: fuiz::settings::Settings,
}

#[derive(serde::Deserialize, Validate)]
#[garde(context(fuiz::settings::Settings))]
struct GameRequest {
    #[garde(dive)]
    config: Fuiz,
    #[garde(dive)]
    options: Options,
}

#[post("/add")]
async fn add(data: Data<AppState>, request: web::Json<GameRequest>) -> impl Responder {
    let request = request.into_inner();

    if let Err(e) = request.validate_with(&data.settings) {
        return HttpResponse::BadRequest().body(e.to_string());
    }

    let GameRequest { config, options } = request;

    let host_id = Id::new();
    let game_id = data.game_manager.add_game(config, options, host_id, &data.settings);

    // Stale Detection
    actix_web::rt::spawn(async move {
        loop {
            actix_web::rt::time::sleep(Duration::from_secs(60)).await;
            match data.game_manager.is_game_done(game_id) {
                Ok(false) => {}
                Ok(true) => {
                    info!("clearing, {game_id}");
                    data.game_manager.remove_game(game_id);
                }
                _ => break,
            }
        }
    });

    HttpResponse::Ok().json(json!({
        "game_id": game_id,
        "watcher_id": host_id
    }))
}

#[get("/alive/{game_id}")]
async fn alive(data: web::Data<AppState>, game_id: web::Path<GameId>) -> impl Responder {
    data.game_manager.exists(game_id.into_inner()).is_ok().to_string()
}

fn websocket_heartbeat_verifier(mut session: actix_ws::Session) -> impl Fn(bytes::Bytes) -> bool {
    let latest_value = Arc::new(AtomicU64::new(0));

    let sender_latest_value = latest_value.clone();
    actix_web::rt::spawn(async move {
        loop {
            actix_web::rt::time::sleep(Duration::from_secs(5)).await;
            let new_value = fastrand::u64(0..u64::MAX);
            sender_latest_value.store(new_value, Ordering::SeqCst);
            if session.ping(&new_value.to_ne_bytes()).await.is_err() {
                break;
            }
        }
    });

    move |bytes: bytes::Bytes| {
        let last_value = latest_value.load(Ordering::SeqCst);
        if let Ok(actual_bytes) = bytes.into_iter().collect::<Vec<_>>().try_into()
            && u64::from_ne_bytes(actual_bytes) == last_value
        {
            return false;
        }
        true
    }
}

#[get("/watch/{game_id}/{watcher_id}")]
#[allow(clippy::too_many_lines)]
async fn watch(
    data: web::Data<AppState>,
    req: HttpRequest,
    body: web::Payload,
    params: web::Path<(GameId, String)>,
) -> Result<HttpResponse, actix_web::Error> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, body)?;

    let (game_id, _) = *params;

    data.game_manager.exists(game_id)?;

    let own_session = Session::new(session.clone());

    let mismatch = websocket_heartbeat_verifier(session.clone());

    let data_thread = data.clone();

    actix_web::rt::spawn(async move {
        let schedule_thread = data_thread.clone();

        let schedule_message: Arc<OnceLock<MessageScheduler>> = Arc::default();

        let thread_schedule_message = schedule_message.clone();

        let temp_schedule_message = move |alarm_message: fuiz::AlarmMessage, duration: Duration| {
            let schedule_thread = schedule_thread.clone();
            let schedule_message = thread_schedule_message.clone();
            actix_web::rt::spawn(async move {
                actix_web::rt::time::sleep(duration).await;
                let _ = schedule_thread
                    .game_manager
                    .receive_alarm(game_id, &alarm_message, |alarm, duration| {
                        schedule_message.get().expect("schedule is unintialized")(alarm, duration);
                    });
            });
        };

        schedule_message
            .as_ref()
            .get_or_init(|| Box::new(temp_schedule_message));

        let mut watcher_id = None;
        while let Some(Ok(msg)) = msg_stream.next().await {
            if data.game_manager.exists(game_id).is_err() {
                break;
            }

            match msg {
                actix_ws::Message::Pong(bytes) => {
                    if mismatch(bytes) {
                        break;
                    }
                }
                actix_ws::Message::Ping(bytes) => {
                    if session.pong(&bytes).await.is_err() {
                        break;
                    }
                }
                actix_ws::Message::Text(s) => {
                    if let Ok(message) = serde_json::from_str(s.as_ref()) {
                        match watcher_id {
                            None => match message {
                                IncomingMessage::Ghost(IncomingGhostMessage::ClaimId(id))
                                    if matches!(data_thread.game_manager.watcher_exists(game_id, id), Ok(true)) =>
                                {
                                    data_thread.game_manager.set_tunnel(id, own_session.clone());

                                    if data_thread.game_manager.update_session(game_id, id).is_err() {
                                        break;
                                    }

                                    watcher_id = Some(id);
                                }
                                IncomingMessage::Ghost(_) => {
                                    let new_id = Id::new();
                                    watcher_id = Some(new_id);

                                    own_session.send_message(&UpdateMessage::IdAssign(new_id).into());

                                    data_thread.game_manager.set_tunnel(new_id, own_session.clone());

                                    match data_thread.game_manager.add_unassigned(game_id, new_id) {
                                        Err(_) | Ok(Err(_)) => {
                                            own_session.clone().close();
                                        }
                                        _ => {}
                                    }
                                }
                                _ => {}
                            },
                            Some(watcher_id) => match message {
                                IncomingMessage::Ghost(_) => {}
                                message => {
                                    let data_thread = data_thread.clone();
                                    let schedule_message = schedule_message.clone();
                                    actix_web::rt::spawn(async move {
                                        let _ = data_thread.game_manager.receive_message(
                                            game_id,
                                            watcher_id,
                                            message,
                                            |alarm, duration| {
                                                schedule_message.get().expect("schedule is unintialized")(
                                                    alarm, duration,
                                                );
                                            },
                                        );
                                    });
                                }
                            },
                        }
                    }
                }
                _ => break,
            }
        }

        if let Some(watcher_id) = watcher_id {
            let _ = data.game_manager.watcher_left(game_id, watcher_id);
            data.game_manager.remove_tunnel(watcher_id);
        }
        session.close(None).await.ok();
    });

    Ok(response)
}

fn server_figment() -> Figment {
    Figment::new()
        .merge(Serialized::defaults(ServerConfig::default()))
        .merge(Env::prefixed("FUIZ_"))
}

fn settings_figment() -> Figment {
    Figment::new()
        .merge(fuiz::settings::Settings::default())
        .merge(Env::prefixed("FUIZ_SETTINGS_").split("__"))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    pretty_env_logger::init();

    let server_config: ServerConfig = server_figment()
        .extract()
        .expect("server configuration should be valid");

    let settings: fuiz::settings::Settings = settings_figment().extract().expect("game settings should be valid");

    let app_state = web::Data::new(AppState {
        game_manager: GameManager::default(),
        settings,
    });

    let origins = server_config.allowed_origins;

    HttpServer::new(move || {
        let app = App::new()
            .wrap(Logger::default())
            .app_data(app_state.clone())
            .route("/hello", web::get().to(|| async { "Hello World!" }))
            .service(alive)
            .service(add)
            .service(watch);

        let mut cors = actix_cors::Cors::default()
            .allowed_methods(vec!["GET", "POST"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
            ])
            .allowed_header(actix_web::http::header::CONTENT_TYPE);
        if origins.is_empty() {
            cors = cors.allow_any_origin();
        } else {
            for origin in &origins {
                cors = cors.allowed_origin(origin);
            }
        }
        app.wrap(cors)
    })
    .bind((server_config.hostname.as_str(), server_config.port))?
    .run()
    .await
}

#[cfg(test)]
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    #[test]
    fn server_config_defaults() {
        figment::Jail::expect_with(|_| {
            let config: ServerConfig = server_figment().extract()?;
            assert_eq!(config.hostname, "0.0.0.0");
            assert_eq!(config.port, 8080);
            assert!(config.allowed_origins.is_empty());
            Ok(())
        });
    }

    #[test]
    fn server_config_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("FUIZ_HOSTNAME", "127.0.0.1");
            jail.set_env("FUIZ_PORT", "9000");

            let config: ServerConfig = server_figment().extract()?;
            assert_eq!(config.hostname, "127.0.0.1");
            assert_eq!(config.port, 9000);
            Ok(())
        });
    }

    #[test]
    fn server_config_multiple_origins() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("FUIZ_ALLOWED_ORIGINS", r#"["https://fuiz.org","https://example.com"]"#);

            let config: ServerConfig = server_figment().extract()?;
            assert_eq!(config.allowed_origins, vec!["https://fuiz.org", "https://example.com"]);
            Ok(())
        });
    }

    #[test]
    fn settings_defaults() {
        figment::Jail::expect_with(|_| {
            let settings: fuiz::settings::Settings = settings_figment().extract()?;
            assert_eq!(settings.fuiz.max_player_count, 1000);
            assert_eq!(settings.question.max_time_limit, 240);
            Ok(())
        });
    }

    #[test]
    fn settings_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("FUIZ_SETTINGS_FUIZ__MAX_PLAYER_COUNT", "500");
            jail.set_env("FUIZ_SETTINGS_QUESTION__MAX_TIME_LIMIT", "300");

            let settings: fuiz::settings::Settings = settings_figment().extract()?;
            assert_eq!(settings.fuiz.max_player_count, 500);
            assert_eq!(settings.question.max_time_limit, 300);
            Ok(())
        });
    }
}
