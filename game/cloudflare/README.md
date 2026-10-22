# Fuiz Cloudflare

Serverless deployment of Fuiz on Cloudflare Workers.

## Why Serverless?

Latency matters for live games. Serverless runs close to players without always-on servers, and provides natural sandboxing: a crash only affects a single game.

## Architecture

Uses two [Durable Objects](https://developers.cloudflare.com/durable-objects/):

- **Game instance**: manages a single game and its WebSocket connections
- **Coordinator**: generates and retrieves game IDs

An image server is also required; see [`corkboard/cloudflare`](../../corkboard/cloudflare/).

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
