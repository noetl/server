# Changelog

All notable changes to this project will be documented in this file.

## [2.0.0](https://github.com/noetl/server/compare/v1.0.3...v2.0.0) (2026-05-31)

### ⚠ BREAKING CHANGES

* **events:** rename `name` to `event_type` + accept executor envelope shape (R-1.2 PR-EE-2)

### Features

* **events:** rename `name` to `event_type` + accept executor envelope shape (R-1.2 PR-EE-2) ([7607dad](https://github.com/noetl/server/commit/7607dad1eea563cd08533094e67909555bcfaf6f)), closes [noetl/ai-meta#30](https://github.com/noetl/ai-meta/issues/30) [noetl/ai-meta#30](https://github.com/noetl/ai-meta/issues/30) [noetl/cli#37](https://github.com/noetl/cli/issues/37)

## [1.0.3](https://github.com/noetl/server/compare/v1.0.2...v1.0.3) (2026-03-02)

### Bug Fixes

* make server image publish resilient and add Dockerfile ([b7cf274](https://github.com/noetl/server/commit/b7cf274d97e5eda04f258a2470306faa68851889))

## [1.0.2](https://github.com/noetl/server/compare/v1.0.1...v1.0.2) (2026-03-02)

### Bug Fixes

* remove secret expressions from workflow conditions ([f5624f0](https://github.com/noetl/server/commit/f5624f07b09035d048579cf4945f24ebb4751e7f))

## [1.0.1](https://github.com/noetl/server/compare/v1.0.0...v1.0.1) (2026-03-02)

### Bug Fixes

* make release input parsing event-safe ([0ec235b](https://github.com/noetl/server/commit/0ec235b36437b582f6f04c7fcf079959a9ace509))

## 1.0.0 (2026-03-02)

### Bug Fixes

* release workflows on push and semantic auth ([978b49a](https://github.com/noetl/server/commit/978b49aea5ad0da8af2720616eb512468f425aa9))
