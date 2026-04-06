# CLI Reference

The `camber` CLI currently exposes three commands.

## `camber new`

Create a new Camber project from a template.

```sh
camber new my-service --template http
```

Arguments:

- `name` — project directory name
- `--template` — template name, defaults to `http`

## `camber serve`

Run the config-driven reverse proxy.

```sh
camber serve config.toml
```

This is the operator-facing entrypoint for the homelab/internal proxy described in `../guides/proxy-quickstart.md`.

Top-level config fields:

- `listen`
- `connection_limit`
- `[tls]`
- `[[site]]`

Each site needs at least one of:

- `proxy`
- `root`

`connection_limit = 0` is invalid.

## `camber context`

Generate `llms.txt` API context for editor and LLM-assisted workflows.

```sh
camber context
```
