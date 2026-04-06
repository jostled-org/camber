# Camber Vision

Camber exists for the large middle of Rust services that are IO-bound, not scheduler experiments.

The goal is simple: make it practical to build and ship HTTP services, queue workers, fan-out tools, and internal APIs in Rust without paying the full ceremony cost of a lower-level async stack on day one.

## What Camber Is

Camber is an opinionated library and project tool built on Tokio.

- Async HTTP handlers with minimal setup
- A scoped runtime for structured concurrency
- Built-in HTTP client, middleware, channels, and resource lifecycle hooks
- A CLI for scaffolding projects and running the homelab proxy

## What Camber Optimizes For

- Small, readable service code
- One obvious way to do common IO-bound work
- Real Rust types and errors, not hidden magic
- A migration path that stays inside the Rust ecosystem

## What Camber Is Not

- Not a general replacement for Tokio
- Not a zero-copy networking toolkit
- Not a Tower-compatible abstraction layer
- Not, today, a production edge-proxy replacement

If you need fine-grained async control everywhere, custom transport work, or heavy Tower-style composition, use Tokio and its surrounding ecosystem directly.

## Who It Is For

- Rust engineers who want less ceremony for everyday services
- Teams using Go for middleware and Rust for hot paths who want one language instead
- Operators who want to try the config-driven proxy in homelab or internal deployments

## How To Read The Docs

- Start with `README.md` for the overview
- Use `docs/guides/` for migration and task-oriented walkthroughs
- Use `docs/reference/` for the supported library and CLI surface
