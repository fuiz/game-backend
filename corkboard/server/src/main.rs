//! Corkboard image server.

use actix_multipart::form::{MultipartForm, MultipartFormConfig};
use actix_web::{App, HttpResponse, HttpServer, Responder, error::ErrorNotFound, get, http::StatusCode, post, web};
use figment::{
    Figment,
    providers::{Env, Serialized},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use storage::{MediaId, Memory};
use thiserror::Error;

mod storage;

/// Server configuration loaded from environment variables with sensible defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ServerConfig {
    /// Hostname to bind to.
    hostname: String,
    /// Port to bind to.
    port: u16,
    /// Allowed CORS origins. Empty means permissive.
    allowed_origins: Vec<String>,
    /// Directory for storing uploaded images on disk.
    storage_dir: PathBuf,
    /// Maximum in-memory cache size in bytes.
    cache_size: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            hostname: "0.0.0.0".into(),
            port: 5040,
            allowed_origins: Vec::new(),
            storage_dir: PathBuf::from("./corkboard-data"),
            cache_size: 256 * 1024 * 1024, // 256 MiB
        }
    }
}

struct AppState {
    storage: Memory,
}

#[get("/get/{media_id}")]
async fn get_full(data: web::Data<AppState>, path: web::Path<MediaId>) -> impl Responder {
    match data.storage.retrieve(path.into_inner()).await {
        Some((bytes, content_type)) => Ok(HttpResponse::build(StatusCode::OK)
            .content_type(content_type)
            // stored bytes come from users, so browsers must not treat them as anything but an image
            .insert_header(("x-content-type-options", "nosniff"))
            .body(bytes)),
        None => Err(ErrorNotFound("This file was not found")),
    }
}

#[derive(MultipartForm)]
struct ImageUpload {
    #[multipart(limit = "25 MiB")]
    image: actix_multipart::form::bytes::Bytes,
}

#[derive(Error, Debug)]
enum ImageDecodingError {
    #[error("type is not recognizable")]
    NotRecognizable(#[from] std::io::Error),
    #[error("{0}")]
    StorageError(#[from] corkboard::StorageError),
}

impl actix_web::error::ResponseError for ImageDecodingError {
    fn status_code(&self) -> StatusCode {
        match self {
            Self::NotRecognizable(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::StorageError(e) => StatusCode::from_u16(e.status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
}

#[post("/thumbnail")]
async fn thumbnail(image_upload: MultipartForm<ImageUpload>) -> impl Responder {
    let ImageUpload { image } = image_upload.into_inner();

    let thumbnail = corkboard::generate_thumbnail(&image.data)?;

    Ok::<HttpResponse, ImageDecodingError>(
        HttpResponse::build(StatusCode::OK)
            .content_type(thumbnail.content_type)
            .body(thumbnail.bytes),
    )
}

#[post("/upload")]
async fn upload(data: web::Data<AppState>, image_upload: MultipartForm<ImageUpload>) -> impl Responder {
    let ImageUpload { image } = image_upload.into_inner();

    let corkboard::StoredImage { bytes, content_type } = corkboard::prepare_for_storage(Vec::from(image.data))?;
    let content_type = content_type.parse().unwrap_or(mime::IMAGE_PNG);

    let media_id = MediaId::new();
    data.storage
        .store(media_id, actix_web::web::Bytes::from(bytes), content_type)
        .await?;

    actix_web::rt::spawn(async move {
        actix_web::rt::time::sleep(std::time::Duration::from_hours(1)).await;
        data.storage.delete(media_id).await;
    });

    Ok::<actix_web::web::Json<MediaId>, ImageDecodingError>(web::Json(media_id))
}

fn server_figment() -> Figment {
    Figment::new()
        .merge(Serialized::defaults(ServerConfig::default()))
        .merge(Env::prefixed("CORKBOARD_"))
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    pretty_env_logger::init();

    let server_config: ServerConfig = server_figment()
        .extract()
        .expect("server configuration should be valid");

    let storage = Memory::new(&server_config.storage_dir, server_config.cache_size)
        .expect("failed to initialize storage directory");
    let app_data = web::Data::new(AppState { storage });

    let origins = server_config.allowed_origins;

    HttpServer::new(move || {
        let app = App::new()
            .app_data(web::PayloadConfig::new(250_000_000))
            .app_data(MultipartFormConfig::default().memory_limit(20_000_000))
            .app_data(app_data.clone())
            .route("/hello", web::get().to(|| async { "Hello World!" }))
            .service(get_full)
            .service(thumbnail)
            .service(upload);

        let mut cors = actix_cors::Cors::default()
            .allowed_methods(vec!["GET", "POST"])
            .allowed_headers(vec![
                actix_web::http::header::AUTHORIZATION,
                actix_web::http::header::ACCEPT,
            ])
            .supports_credentials()
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
            assert_eq!(config.port, 5040);
            assert_eq!(config.allowed_origins, [] as [String; 0]);
            Ok(())
        });
    }

    #[test]
    fn server_config_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("CORKBOARD_HOSTNAME", "127.0.0.1");
            jail.set_env("CORKBOARD_PORT", "6000");

            let config: ServerConfig = server_figment().extract()?;
            assert_eq!(config.hostname, "127.0.0.1");
            assert_eq!(config.port, 6000);
            Ok(())
        });
    }
}
