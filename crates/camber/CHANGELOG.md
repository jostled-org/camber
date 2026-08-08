# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.1](https://github.com/jostled-org/camber/compare/camber-v0.4.0...camber-v0.4.1) - 2026-08-08

### Other

- *(proxy)* synchronize committed stream truncation

## [0.4.0](https://github.com/jostled-org/camber/compare/camber-v0.3.0...camber-v0.4.0) - 2026-08-08

### Added

- [**breaking**] implement structured-framework-rejections

## [0.3.0](https://github.com/jostled-org/camber/compare/camber-v0.2.2...camber-v0.3.0) - 2026-08-02

### Added

- implement raw-request-identity
- [**breaking**] implement runtime-ownership-and-disconnect-cancellation

### Fixed

- [**breaking**] harden runtime ownership and I/O boundaries
- *(http)* await both proxy websocket close replies
- *(release)* decouple workspace package versions

### Other

- implement raw-request-identity-proof-contract
- *(deps)* update rust dependency graph
- fix release-plz and pedant test-tree failures

## [0.2.2](https://github.com/jostled-org/camber/compare/camber-v0.2.1...camber-v0.2.2) - 2026-07-24

### Other

- *(http)* extract shared streaming response builder
- *(http)* split streaming and request recording into modules

## [0.2.1](https://github.com/jostled-org/camber/compare/camber-v0.2.0...camber-v0.2.1) - 2026-07-23

### Other

- repair the container image build
- stop test busy-waits from starving the runner

## [0.2.0](https://github.com/jostled-org/camber/compare/camber-v0.1.8...camber-v0.2.0) - 2026-07-22

### Changed

- Harden runtime shutdown, transport ownership, and lifecycle integration contracts.

### Fixed

- Prevent synchronous runtime loops from missing shutdown wakeups.

## [0.1.8](https://github.com/jostled-org/camber/compare/camber-v0.1.7...camber-v0.1.8) - 2026-07-18

### Added

- *(http)* add owned server lifecycle control

### Fixed

- *(ci)* stabilize warning-clean test suite
- *(ci)* satisfy question-mark lint

### Other

- stabilize pool backpressure coverage
- *(deps)* refresh workspace dependencies

## [0.1.7](https://github.com/jostled-org/camber/compare/camber-v0.1.6...camber-v0.1.7) - 2026-06-07

### Other

- *(deps)* update all dependencies including breaking bumps

## [0.1.6](https://github.com/jostled-org/camber/compare/camber-v0.1.5...camber-v0.1.6) - 2026-04-24

### Added

- *(channel)* add watch channel, supply chain CI, and Tokio boundary docs

## [0.1.5](https://github.com/jostled-org/camber/compare/camber-v0.1.4...camber-v0.1.5) - 2026-04-07

### Fixed

- tighten minimum versions to exclude known vulnerabilities

## [0.1.4](https://github.com/jostled-org/camber/compare/camber-v0.1.3...camber-v0.1.4) - 2026-04-07

### Fixed

- upgrade all breaking dependencies to latest

### Other

- Update README.md

## [0.1.2](https://github.com/jostled-org/camber/compare/camber-v0.1.1...camber-v0.1.2) - 2026-04-07

### Fixed

- update deps
- common/mod.rs fmt
- update rustls deps
