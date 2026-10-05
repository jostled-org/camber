# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Breaking changes

- *(serve)* `camber serve` validates the whole config before it has any
  effect. A file that ran before can now be refused:
  - Unknown fields fail, at the top level, in `[tls]`, and in each `[[site]]`.
  - Two sites with the same canonical host fail, even when their ports differ.
  - `proxy` must be an `http` or `https` URL without credentials, a query, or
    a fragment.
  - `health_check` requires `proxy`. `health_interval` requires
    `health_check`.
  - `dns_provider` accepts only `"cloudflare"`. `[tls]` refuses fields that
    its mode does not use.
  - The file must declare at least one `[[site]]`.
  - `host` must be a canonical DNS name or an IP address, with an optional
    port.
  - `root` must be a readable directory.
  - Under `auto = true`, a site whose host is an IP address fails.
- *(serve)* `camber serve --help` lists every accepted field and the proxy URL
  constraints.
- *(config)* `Config` and `SiteConfig` no longer implement `Deserialize`. Call
  `Config::load` to get a validated configuration.
- *(config)* `Config::tls` returns the parsed `camber::config::TlsMode`, not
  `TlsConfig`. Match on the mode. The `camber_cli::config::TlsConfig`
  re-export is removed.

### Fixed

- *(serve)* DNS-01 prepares the zone of every site host, not only the first.
- *(serve)* A site host with a port routes by its hostname.
- *(serve)* An overlay site's local-file miss answers 503 while its upstream
  health check fails, the same as its proxy routes.

## [0.8.5](https://github.com/jostled-org/camber/compare/camber-cli-v0.8.4...camber-cli-v0.8.5) - 2026-09-26

### Other

- updated the following local packages: camber

## [0.8.4](https://github.com/jostled-org/camber/compare/camber-cli-v0.8.3...camber-cli-v0.8.4) - 2026-09-26

### Fixed

- stabilize compatibility checks and streaming failure tests

### Other

- *(release)* constrain semver checks to supported features

## [0.8.3](https://github.com/jostled-org/camber/compare/camber-cli-v0.8.2...camber-cli-v0.8.3) - 2026-08-29

### Other

- updated the following local packages: camber

## [0.8.2](https://github.com/jostled-org/camber/compare/camber-cli-v0.8.1...camber-cli-v0.8.2) - 2026-08-29

### Other

- updated the following local packages: camber

## [0.8.1](https://github.com/jostled-org/camber/compare/camber-cli-v0.8.0...camber-cli-v0.8.1) - 2026-08-21

### Other

- updated the following local packages: camber

## [0.8.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.7.0...camber-cli-v0.8.0) - 2026-08-21

### Added

- *(runtime)* [**breaking**] share one deadline across shutdown owners
- *(http)* [**breaking**] read a served file off the worker under a frozen maximum
- implement shared-immutable-websocket-payloads

### Fixed

- *(runtime)* [**breaking**] bound and report the transport edges serving left open

## [0.7.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.6.0...camber-cli-v0.7.0) - 2026-08-15

### Added

- [**breaking**] implement independent-websocket-directions

## [0.6.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.5.2...camber-cli-v0.6.0) - 2026-08-14

### Added

- [**breaking**] implement bounded-streaming-multipart

## [0.5.2](https://github.com/jostled-org/camber/compare/camber-cli-v0.5.1...camber-cli-v0.5.2) - 2026-08-11

### Other

- updated the following local packages: camber

## [0.5.1](https://github.com/jostled-org/camber/compare/camber-cli-v0.5.0...camber-cli-v0.5.1) - 2026-08-11

### Other

- updated the following local packages: camber

## [0.5.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.4.2...camber-cli-v0.5.0) - 2026-08-10

### Added

- [**breaking**] implement route-aware-body-admission

## [0.4.2](https://github.com/jostled-org/camber/compare/camber-cli-v0.4.1...camber-cli-v0.4.2) - 2026-08-08

### Other

- updated the following local packages: camber

## [0.4.1](https://github.com/jostled-org/camber/compare/camber-cli-v0.4.0...camber-cli-v0.4.1) - 2026-08-08

### Other

- updated the following local packages: camber

## [0.4.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.3.0...camber-cli-v0.4.0) - 2026-08-08

### Added

- [**breaking**] implement structured-framework-rejections

## [0.3.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.2.2...camber-cli-v0.3.0) - 2026-08-02

### Fixed

- [**breaking**] harden runtime ownership and I/O boundaries
- *(release)* decouple workspace package versions

## [0.2.2](https://github.com/jostled-org/camber/compare/camber-cli-v0.2.1...camber-cli-v0.2.2) - 2026-07-24

### Other

- updated the following local packages: camber

## [0.2.1](https://github.com/jostled-org/camber/compare/camber-cli-v0.2.0...camber-cli-v0.2.1) - 2026-07-23

### Other

- updated the following local packages: camber

## [0.2.0](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.8...camber-cli-v0.2.0) - 2026-07-22

### Other

- Synchronize the workspace release at version 0.2.0.

## [0.1.8](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.7...camber-cli-v0.1.8) - 2026-07-18

### Fixed

- *(ci)* stabilize warning-clean test suite

## [0.1.7](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.6...camber-cli-v0.1.7) - 2026-06-07

### Other

- updated the following local packages: camber

## [0.1.6](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.5...camber-cli-v0.1.6) - 2026-04-24

### Other

- updated the following local packages: camber

## [0.1.5](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.4...camber-cli-v0.1.5) - 2026-04-07

### Other

- updated the following local packages: camber

## [0.1.4](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.3...camber-cli-v0.1.4) - 2026-04-07

### Fixed

- upgrade all breaking dependencies to latest

### Other

- Update README.md

## [0.1.2](https://github.com/jostled-org/camber/compare/camber-cli-v0.1.1...camber-cli-v0.1.2) - 2026-04-07

### Other

- update Cargo.lock dependencies
