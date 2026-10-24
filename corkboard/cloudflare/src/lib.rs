//! Corkboard Cloudflare worker.

use serde_hex::{SerHex, Strict};
use serde_json::json;
use worker::*;

const IMAGE_EXPIRATION: std::time::Duration = std::time::Duration::from_hours(24);

/// Largest value Workers KV stores.
const MAX_KV_VALUE_BYTES: usize = 25 * 1024 * 1024;

/// Handles incoming fetch events.
///
/// # Errors
/// Returns a [`worker::Error`] if the request cannot be processed.
#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: worker::Context) -> Result<Response> {
    let router = Router::new();

    router
        .get("/hello", |_, _| Response::ok("Hello World!"))
        .post_async("/thumbnail", |mut req, _ctx| async move {
            let Some(image) = req.form_data().await?.get("image") else {
                return Response::error("no image in request", 400);
            };

            let data = match image {
                FormEntry::File(f) => f.bytes().await?,
                FormEntry::Field(_) => return Response::error("image has to be a file", 400),
            };

            let thumbnail = match corkboard::generate_thumbnail(&data) {
                Ok(thumbnail) => thumbnail,
                Err(e) => return Response::error(format!("failed to generate thumbnail: {e}"), e.status_code()),
            };

            Ok(
                Response::from_body(worker::ResponseBody::Body(thumbnail.bytes))?.with_headers({
                    let headers = Headers::new();
                    headers.append("content-type", thumbnail.content_type)?;
                    headers
                }),
            )
        })
        .post_async("/upload", |mut req, ctx| async move {
            let Some(image) = req.form_data().await?.get("image") else {
                return Response::error("no image in request", 400);
            };

            let data = match image {
                FormEntry::File(f) => f.bytes().await?,
                FormEntry::Field(_) => return Response::error("image has to be a file", 400),
            };

            let image = match corkboard::prepare_for_storage(data) {
                Ok(image) => image,
                Err(e) => return Response::error(e.to_string(), e.status_code()),
            };

            if image.bytes.len() > MAX_KV_VALUE_BYTES {
                return Response::error("image is larger than 25 MiB", 413);
            }

            let kv = ctx.kv("IMAGES")?;

            let Ok(random_key) = getrandom::u64() else {
                return Response::error("couldn't generate random key", 500);
            };

            let Ok(key) = <u64 as SerHex<Strict>>::into_hex(&random_key) else {
                return Response::error("couldn't convert hash to hex", 500);
            };

            kv.put_bytes(&key, &image.bytes)?
                .metadata(image.content_type)?
                .expiration_ttl(IMAGE_EXPIRATION.as_secs())
                .execute()
                .await?;

            Response::from_json(&json!(key))
        })
        .get_async("/get/:media_id", |_req, ctx| async move {
            let Some(media_id) = ctx.param("media_id") else {
                return Response::error("missing media_id", 400);
            };

            let kv = ctx.kv("IMAGES")?;

            let (bytes, content_type) = kv.get(media_id).bytes_with_metadata().await?;

            Ok(Response::from_bytes(bytes.unwrap_or_default())?.with_headers({
                Headers::from_iter([
                    (
                        "content-type".to_string(),
                        content_type.unwrap_or("image/png".to_string()),
                    ),
                    // stored bytes come from users, so browsers must not treat them as anything but an image
                    ("x-content-type-options".to_string(), "nosniff".to_string()),
                ])
            }))
        })
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
