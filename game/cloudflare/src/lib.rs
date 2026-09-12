//! Fuiz game Cloudflare worker.

mod game;
mod game_manager;
mod journal;

use garde::Validate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use wasm_bindgen_futures::wasm_bindgen::JsValue;
use worker::*;

/// A game manager durable object instance reference.
#[derive(Serialize, Deserialize)]
pub struct GameManagerInstance {
    /// The durable object ID.
    pub id: String,
    /// When the instance was created.
    pub created_at: chrono::DateTime<chrono::Utc>,
}

async fn fetch_instance(env: &Env, game_id: &str) -> Result<GameManagerInstance> {
    let game_manager = env
        .durable_object("GAME_MANAGER")?
        .id_from_name("default")?
        .get_stub()?;

    let mut response = game_manager
        .fetch_with_str(&format!("https://example.com/{game_id}"))
        .await?;

    let game_manager_instance = response.json().await?;

    Ok(game_manager_instance)
}

async fn start_instance(env: &Env, game_manager_instance: &GameManagerInstance) -> Result<String> {
    console_log!("Preparing request to game manager");

    let request = Request::new_with_init(
        "http://example.com",
        &RequestInit {
            method: Method::Post,
            headers: {
                let headers = Headers::new();
                headers.append("content-type", "application/json")?;
                headers
            },
            body: Some(JsValue::from_str(&serde_json::to_string(&game_manager_instance)?)),
            ..RequestInit::default()
        },
    )?;

    console_log!("Creating game instance");

    let game_manager = env
        .durable_object("GAME_MANAGER")?
        .id_from_name("default")?
        .get_stub()?;

    let mut response = game_manager
        .fetch_with_request(request)
        .await
        .map_err(|e| Error::RustError(e.to_string()))?;

    console_log!("Parsing game ID from response");

    let game_id = response.json().await?;

    Ok(game_id)
}

#[event(fetch)]
#[allow(clippy::too_many_lines)]
async fn fetch(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let router = Router::new();

    router
        .get("/hello", |_, _| Response::ok("Hello World!"))
        .post_async("/add", |mut req, ctx| async move {
            console_log!("Received /add request");

            let game_request = match req.json::<game::GameRequest>().await {
                Ok(game_request) => game_request,
                Err(e) => {
                    console_error!("Error parsing request: {:?}", e);
                    return Response::error(format!("Invalid request: {e}"), 400);
                }
            };

            let settings = fuiz::settings::Settings::default();
            if let Err(e) = game_request.validate_with(&settings) {
                return Response::error(e.to_string(), 400);
            }

            console_log!("Creating game with: {:?}", game_request);

            let game_namespace = ctx.durable_object("GAME")?;

            let internal_id = game_namespace.unique_id()?;

            let game_manager_instance = GameManagerInstance {
                id: internal_id.to_string(),
                created_at: chrono::Utc::now(),
            };

            let game_id = start_instance(&ctx.env, &game_manager_instance).await.map_err(|e| {
                console_error!("Error storing game instance: {:?}", e);
                Error::RustError("Failed to store game instance".to_string())
            })?;

            console_log!("Created game with ID: {}", game_id);

            let stub = internal_id.get_stub()?;

            let watcher_id = stub
                .fetch_with_request(Request::new_with_init(
                    "http://example.com/add",
                    &RequestInit {
                        method: Method::Post,
                        body: Some(JsValue::from_str(
                            &serde_json::to_string(&game_request).expect("serializer failed"),
                        )),
                        headers: {
                            let headers = Headers::new();
                            headers.append("content-type", "application/json")?;
                            headers
                        },
                        ..Default::default()
                    },
                )?)
                .await
                .map_err(|e| {
                    console_error!("Error creating game: {:?}", e);
                    Error::RustError("Failed to create game".to_string())
                })?
                .text()
                .await
                .map_err(|e| {
                    console_error!("Error fetching watcher ID: {:?}", e);
                    Error::RustError("Failed to fetch watcher ID".to_string())
                })?;

            Response::from_json(&json!({
                "watcher_id": watcher_id,
                "game_id": game_id
            }))
        })
        .get_async("/watch/:gameid/:watcherid", |req, ctx| async move {
            let Some(id) = ctx.param("gameid") else {
                return Response::error("Bad Request", 400);
            };

            let Ok(game_instance) = fetch_instance(&ctx.env, id).await else {
                return Response::error("Not Found", 404);
            };

            let game_stub = ctx
                .durable_object("GAME")?
                .id_from_string(&game_instance.id)?
                .get_stub()?;

            game_stub.fetch_with_request(req).await
        })
        .get_async("/watch/:gameid/", |req, ctx| async move {
            let Some(id) = ctx.param("gameid") else {
                return Response::error("Bad Request", 400);
            };

            let Ok(game_instance) = fetch_instance(&ctx.env, id).await else {
                return Response::error("Not Found", 404);
            };

            let namespace = ctx.durable_object("GAME")?;

            let stub = namespace.id_from_string(&game_instance.id)?.get_stub()?;

            stub.fetch_with_request(req).await
        })
        .get_async("/alive/:gameid", |req, ctx| async move {
            let Some(id) = ctx.param("gameid") else {
                return Response::error("Bad Request", 400);
            };

            let Ok(game_instance) = fetch_instance(&ctx.env, id).await else {
                return Response::ok("false");
            };

            let namespace = ctx.durable_object("GAME")?;

            let stub = namespace.id_from_string(&game_instance.id)?.get_stub()?;

            stub.fetch_with_request(req).await
        })
        .options("/add", |_, _| Response::ok(""))
        .run(req, env)
        .await?
        .with_cors(
            &Cors::default()
                .with_max_age(86400)
                .with_allowed_headers(["*"])
                .with_origins(vec!["https://fuiz.org"])
                .with_methods(vec![Method::Get, Method::Post, Method::Options]),
        )
}
