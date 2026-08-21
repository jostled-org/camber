# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.1](https://github.com/jostled-org/camber/compare/camber-v0.8.0...camber-v0.8.1) - 2026-08-21

### Other

- *(websocket)* order cancellation before transport teardown

## [0.8.0](https://github.com/jostled-org/camber/compare/camber-v0.7.0...camber-v0.8.0) - 2026-08-21

### Added

- *(runtime)* [**breaking**] name the outermost failed owner as the primary
- *(runtime)* [**breaking**] share one deadline across shutdown owners
- *(runtime)* [**breaking**] bound every resource callback to one owning coordinator
- *(runtime)* [**breaking**] name the owner every lifecycle failure belongs to
- *(http)* record each request where its operation actually ends
- *(http)* carry each gated chain's stated head onto the real one
- *(codegen)* name the absent wrapper where a skipped one would have been
- *(http)* [**breaking**] bound each gRPC call to the head tonic commits
- *(http)* freeze each proxy route's upstream owner with its graph
- *(http)* bound each streaming direction under its own transfer owner
- *(http)* [**breaking**] retain a rendered profile under a frozen maximum
- *(http)* [**breaking**] read a served file off the worker under a frozen maximum
- *(http)* [**breaking**] collect a buffered proxy answer under a frozen maximum
- *(http)* [**breaking**] collect the client response under a configured maximum
- *(http)* [**breaking**] bind an admitted request to its configured deadlines
- *(http)* [**breaking**] bound service operation with validated policies
- implement shared-immutable-websocket-payloads

### Fixed

- *(http)* shrink internal refusal errors
- *(runtime)* [**breaking**] bound and report the transport edges serving left open
- *(runtime)* align completion proofs with owner order
- *(http)* wake pre-commit operations on control changes
- *(http)* bind middleware, stream heads, and upgrades to the total
- *(http)* observe disconnect before body source failure
- *(test)* retry the blocking serve row on a lost port
- *(http)* refuse policy bounds their owners cannot carry
- *(http)* align service-operation proof boundaries
- *(test)* close checkpoint wake and address-reuse races
- *(ws)* require supervised bridge registration
- *(ws)* [**breaking**] refuse a handshake that declares a payload

### Other

- *(lifecycle)* await WebSocket close handshake
- *(docs)* assert four more reference-page contracts verbatim
- *(runtime)* quote the child's stderr on both isolated failure paths
- *(runtime)* prove a refused readiness pass served no queued peer
- *(runtime)* match every lifecycle name in one place for both roots
- attach the tracing setup summary to the module it describes
- *(runtime)* name every stage a lifecycle failure can be recorded in
- *(http)* read each account's claims off the values production held
- *(http)* read each stated head off the owner that commits it
- *(http)* hold the escalation off the rows a stop would preempt
- *(http)* read the streaming rows' claims off their transfer owners
- *(http)* prove the defaulted profile renders under the frozen maximum
- *(http)* prove the unnamed spellings freeze the documented maximum
- *(http)* prove the frozen maximum is the route's and holds no crossing
- *(http)* prove the response ceiling opt-out, default, and idle order
- *(http)* widen deadline and envelope proofs to every admitted class
- *(ws)* bound the pending connection-permit wait
- *(net)* drop the unreachable limit from the raw accept loop
- *(http)* hold the synchronous rows at their production checkpoints
- *(test)* reuse the runtime's default worker count in tests
- *(test)* retag lifecycle proofs to their current plan steps
- *(http)* cover a server header narrowing the runtime
- *(http)* rename positive_bytes to positive_limit
- *(test)* format included lifecycle test source
- *(http)* cover limit narrowing, policy ordering, and transfer budgets
- *(http)* share the one-transport server setup across permit rows
- *(ws)* keep precedence observation ahead of forced abort
- *(http)* cover gRPC and bare-Tokio connection-limit rows
- *(ws)* order cancellation before terminal polling

## [0.7.0](https://github.com/jostled-org/camber/compare/camber-v0.6.0...camber-v0.7.0) - 2026-08-15

### Added

- [**breaking**] implement independent-websocket-directions

### Fixed

- *(ws)* [**breaking**] make terminal precedence deterministic
- *(ws)* gate carried runtime helper with websocket feature

### Other

- *(ws)* fix equal-ready checkpoint ordering
- *(multipart)* make HTTP/1 backpressure proof deterministic

## [0.6.0](https://github.com/jostled-org/camber/compare/camber-v0.5.2...camber-v0.6.0) - 2026-08-14

### Added

- [**breaking**] implement bounded-streaming-multipart

## [0.5.2](https://github.com/jostled-org/camber/compare/camber-v0.5.1...camber-v0.5.2) - 2026-08-11

### Other

- updated the following local packages: camber-macros

## [0.5.1](https://github.com/jostled-org/camber/compare/camber-v0.5.0...camber-v0.5.1) - 2026-08-11

### Fixed

- *(http)* preserve streaming body refusal precedence

## [0.5.0](https://github.com/jostled-org/camber/compare/camber-v0.4.2...camber-v0.5.0) - 2026-08-10

### Added

- [**breaking**] implement route-aware-body-admission

## [0.4.2](https://github.com/jostled-org/camber/compare/camber-v0.4.1...camber-v0.4.2) - 2026-08-08

### Other

- *(proxy)* read request before scripted response

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
