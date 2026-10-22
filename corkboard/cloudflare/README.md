# Corkboard Cloudflare

Serverless deployment of Corkboard on Cloudflare Workers. Uses Durable Objects for temporary image storage with 24-hour expiration.

## Prerequisites

`wrangler dev` and `wrangler deploy` both run `worker-build`, so install it first:

```bash
cargo install worker-build --locked --version 0.8.7
rustup target add wasm32-unknown-unknown
```

## Development

```bash
bunx wrangler dev
```

## Deployment

```bash
bunx wrangler deploy
```
