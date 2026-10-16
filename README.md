# Fuiz

A Rust monorepo for the Fuiz live quiz platform: real-time quiz games with multiple question types, team support, and leaderboards.

## Structure

### [`game/`](game/): Game Engine

| Crate                                | Description                                 |
| ------------------------------------ | ------------------------------------------- |
| [`game/logic`](game/logic/)          | Core game engine library (WASM compatible)  |
| [`game/server`](game/server/)        | Self-hostable game server (actix-web)       |
| [`game/cloudflare`](game/cloudflare/)| Serverless deployment on Cloudflare Workers |

### [`corkboard/`](corkboard/): Image Storage

| Crate                                            | Description                                        |
| ------------------------------------------------ | -------------------------------------------------- |
| [`corkboard/logic`](corkboard/logic/)            | Image processing library                           |
| [`corkboard/server`](corkboard/server/)          | Self-hostable image storage server (actix-web)     |
| [`corkboard/cloudflare`](corkboard/cloudflare/) | Serverless image storage on Cloudflare Workers      |

## Getting Started

Requires Rust 1.88+ (Edition 2024).

```bash
cargo build
```

To run the game server locally:

```bash
cargo run -p fuiz-server
```

To run the corkboard server locally:

```bash
cargo run -p corkboard-server
```

## Configuration

Both servers read their settings from environment variables:

| Variable                    | Default            | Description                                            |
| --------------------------- | ------------------ | ------------------------------------------------------ |
| `FUIZ_HOSTNAME`             | `0.0.0.0`          | Game server bind address                               |
| `FUIZ_PORT`                 | `8080`             | Game server port                                       |
| `FUIZ_ALLOWED_ORIGINS`      | `[]`               | Allowed CORS origins as a JSON array; empty allows any |
| `CORKBOARD_HOSTNAME`        | `0.0.0.0`          | Image server bind address                              |
| `CORKBOARD_PORT`            | `5040`             | Image server port                                      |
| `CORKBOARD_ALLOWED_ORIGINS` | `[]`               | Allowed CORS origins as a JSON array; empty allows any |
| `CORKBOARD_STORAGE_DIR`     | `./corkboard-data` | Where uploaded images are stored                       |
| `CORKBOARD_CACHE_SIZE`      | `268435456`        | In-memory image cache size in bytes (256 MiB)          |

Game limits from [settings.rs](game/logic/src/settings.rs) can be overridden with `FUIZ_SETTINGS_`, using `__` between levels, for example `FUIZ_SETTINGS_FUIZ__MAX_PLAYER_COUNT=2000`. The website enforces its own copy of these limits in `src/lib/limits.ts`, so keep the two in sync.

## Self-Hosting

CI publishes both servers as Docker images:

- `registry.gitlab.com/fuiz/game-backend/fuiz-server`
- `registry.gitlab.com/fuiz/game-backend/corkboard-server`

[fuiz/self-hosted](https://gitlab.com/fuiz/self-hosted) runs them together with the website behind Caddy.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).

Developed by [Beyond Expiry](https://beyondexpiry.org/).
