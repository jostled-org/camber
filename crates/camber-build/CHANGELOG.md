# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0](https://github.com/jostled-org/camber/compare/camber-build-v0.5.0...camber-build-v0.6.0) - 2026-08-21

### Added

- *(runtime)* [**breaking**] share one deadline across shutdown owners
- *(codegen)* name the absent wrapper where a skipped one would have been
- *(http)* [**breaking**] bound each gRPC call to the head tonic commits
- implement shared-immutable-websocket-payloads

### Fixed

- *(runtime)* [**breaking**] bound and report the transport edges serving left open

## [0.5.0](https://github.com/jostled-org/camber/compare/camber-build-v0.4.0...camber-build-v0.5.0) - 2026-08-15

### Added

- [**breaking**] implement independent-websocket-directions

## [0.4.0](https://github.com/jostled-org/camber/compare/camber-build-v0.3.0...camber-build-v0.4.0) - 2026-08-14

### Added

- [**breaking**] implement bounded-streaming-multipart

## [0.3.0](https://github.com/jostled-org/camber/compare/camber-build-v0.2.3...camber-build-v0.3.0) - 2026-08-08

### Added

- [**breaking**] implement structured-framework-rejections

## [0.2.3](https://github.com/jostled-org/camber/compare/camber-build-v0.2.2...camber-build-v0.2.3) - 2026-08-02

### Fixed

- *(release)* decouple workspace package versions

## [0.2.1](https://github.com/jostled-org/camber/compare/camber-build-v0.2.0...camber-build-v0.2.1) - 2026-07-23

### Fixed

- *(test)* stop codegen process-tree reaping from aborting on Linux

### Other

- stop test busy-waits from starving the runner

## [0.2.0](https://github.com/jostled-org/camber/compare/camber-build-v0.1.8...camber-build-v0.2.0) - 2026-07-22

### Other

- Synchronize the workspace release at version 0.2.0.

## [0.1.8](https://github.com/jostled-org/camber/compare/camber-build-v0.1.7...camber-build-v0.1.8) - 2026-07-18

### Other

- *(deps)* refresh workspace dependencies

## [0.1.4](https://github.com/jostled-org/camber/compare/camber-build-v0.1.3...camber-build-v0.1.4) - 2026-04-07

### Fixed

- upgrade all breaking dependencies to latest

### Other

- Update README.md

## [0.1.2](https://github.com/jostled-org/camber/compare/camber-build-v0.1.1...camber-build-v0.1.2) - 2026-04-07

### Other

- release v0.1.2
