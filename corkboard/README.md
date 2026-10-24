# Corkboard

Temporary image storage service for Fuiz. Handles image uploads, retrieval, and thumbnail generation.

## Crates

| Crate                          | Description                                        |
| ------------------------------ | -------------------------------------------------- |
| [`logic`](logic/)              | Image processing library                           |
| [`server`](server/)            | Self-hostable image storage server (actix-web)     |
| [`cloudflare`](cloudflare/)    | Serverless image storage on Cloudflare Workers     |

## API

### Upload

```http
POST /upload
```

Accepts a multipart form with an `image` field. Returns a `MediaID`. Images expire after an hour (server) or 24 hours (cloudflare).

JPEG, PNG, GIF, WebP and still AVIF images are stored in their own format with all metadata removed except the color profile and orientation. HEIC, JPEG XL and animated AVIF images are rejected with `415`, since only Safari displays them. Other formats are converted to PNG. Images over 50 million pixels are rejected.

### Retrieve

```http
GET /get/{media_id}
```

Returns the image with the content type it was stored with.

### Compute Thumbnail

```http
POST /thumbnail
```

Accepts a multipart form with an `image` field. Returns a thumbnail as `image/png`. Still AVIF images up to 1 MiB, which can't be decoded, are returned as `image/avif` with their metadata removed.
