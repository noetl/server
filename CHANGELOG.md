# Changelog

All notable changes to this project will be documented in this file.

## [2.19.1](https://github.com/noetl/server/compare/v2.19.0...v2.19.1) (2026-06-05)

### Bug Fixes

* **events:** accept i64 wire shape for execution_id / event_id ([7e65712](https://github.com/noetl/server/commit/7e6571276dddaf0f535d36da1a467f6d88c0bb49)), closes [noetl/ai-meta#55](https://github.com/noetl/ai-meta/issues/55)

## [2.19.0](https://github.com/noetl/server/compare/v2.18.0...v2.19.0) (2026-06-04)

### Features

* **services:** ExecutionService takes DbPoolMap (Phase F R4-4b) ([18cfb74](https://github.com/noetl/server/commit/18cfb740eeb5214b5795286bb82161eba8624930)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.18.0](https://github.com/noetl/server/compare/v2.17.0...v2.18.0) (2026-06-04)

### Features

* **db,handlers:** cross-shard fan-out + event_id resolver (Phase F R4-4) ([86dea97](https://github.com/noetl/server/commit/86dea97e9eabf7460aac9b9186f83e727ebc2d16)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.17.0](https://github.com/noetl/server/compare/v2.16.0...v2.17.0) (2026-06-04)

### Features

* **handlers:** cut health.rs over to DbPoolMap (Phase F R4-3c) ([d4e2aa3](https://github.com/noetl/server/commit/d4e2aa30c18441b80e7849684508883291fef6ad)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.16.0](https://github.com/noetl/server/compare/v2.15.0...v2.16.0) (2026-06-04)

### Features

* **handlers:** cut execute.rs over to DbPoolMap (Phase F R4-3b) ([399ece9](https://github.com/noetl/server/commit/399ece9ae11c7fb38c2ae914333bd72da9685bd2)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.15.0](https://github.com/noetl/server/compare/v2.14.0...v2.15.0) (2026-06-04)

### Features

* **handlers:** cut events.rs over to DbPoolMap (Phase F R4-3a) ([515ed3d](https://github.com/noetl/server/commit/515ed3df0b9c51b7b7b22a054b8c3a6889d07c54)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.14.0](https://github.com/noetl/server/compare/v2.13.0...v2.14.0) (2026-06-04)

### Features

* **state:** wire DbPoolMap into AppState (Phase F R4-2) ([605d738](https://github.com/noetl/server/commit/605d738215af79ba201740663624f7c7aef0a024)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.13.0](https://github.com/noetl/server/compare/v2.12.0...v2.13.0) (2026-06-04)

### Features

* **config,db:** add ShardingConfig + DbPoolMap (Phase F R4-1) ([6b4000c](https://github.com/noetl/server/commit/6b4000ce25a722c5618d78fee1ef8859d845f4d1)), closes [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#48](https://github.com/noetl/server/issues/48) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.12.0](https://github.com/noetl/server/compare/v2.11.0...v2.12.0) (2026-06-04)

### Features

* **sharding:** GET /api/runtime/shard-info diagnostic endpoint (Phase F R3b-1) ([6be4f3a](https://github.com/noetl/server/commit/6be4f3a3617b038aafeea5b825d38b0bdad47c1f)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#46](https://github.com/noetl/server/issues/46) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.11.0](https://github.com/noetl/server/compare/v2.10.1...v2.11.0) (2026-06-04)

### Features

* **sharding:** server-side shard_id() helper + ShardConfig (Phase F R2) ([daa1294](https://github.com/noetl/server/commit/daa129435f16164beba11c943795946184d5bf58)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#44](https://github.com/noetl/server/issues/44) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.10.1](https://github.com/noetl/server/compare/v2.10.0...v2.10.1) (2026-06-04)

### Bug Fixes

* **config:** rename machine_id → server_machine_id (env: NOETL_SERVER_MACHINE_ID) ([a9533aa](https://github.com/noetl/server/commit/a9533aa3d9b22de41b7ca41a86296bf8b7ba8eb8)), closes [noetl/server#42](https://github.com/noetl/server/issues/42) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.10.0](https://github.com/noetl/server/compare/v2.9.0...v2.10.0) (2026-06-04)

### Features

* **snowflake:** app-side snowflake ID generation (Phase F R1.5) ([896d5a1](https://github.com/noetl/server/commit/896d5a13288decac088105f607ef4305bfb4888c)), closes [noetl/server#41](https://github.com/noetl/server/issues/41) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.9.0](https://github.com/noetl/server/compare/v2.8.3...v2.9.0) (2026-06-04)

### Features

* **events:** server adopts noetl-events as canonical envelope (EE-4 PR 3) ([3949fdf](https://github.com/noetl/server/commit/3949fdfc6065b4d66b9457dc2532b4c729dd3215)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/cli#49](https://github.com/noetl/cli/issues/49) [#50](https://github.com/noetl/server/issues/50) [noetl/server#29](https://github.com/noetl/server/issues/29) [noetl/cli#49](https://github.com/noetl/cli/issues/49) [noetl/cli#50](https://github.com/noetl/cli/issues/50) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.8.3](https://github.com/noetl/server/compare/v2.8.2...v2.8.3) (2026-06-04)

### Bug Fixes

* **orchestrator:** R3b iterator end-to-end — args injection + state reconstruction ([77ed29f](https://github.com/noetl/server/commit/77ed29f3012e74aaa13c791beb04189af637624c)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#29](https://github.com/noetl/server/issues/29) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.8.2](https://github.com/noetl/server/compare/v2.8.1...v2.8.2) (2026-06-04)

### Bug Fixes

* **orchestrator:** guard skip-chain target against re-dispatch on re-trigger ([b70e029](https://github.com/noetl/server/commit/b70e029c2219b73cf1a28242b3b2bfd804cd5061)), closes [noetl/ai-meta#53](https://github.com/noetl/ai-meta/issues/53) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#53](https://github.com/noetl/ai-meta/issues/53)

## [2.8.1](https://github.com/noetl/server/compare/v2.8.0...v2.8.1) (2026-06-03)

### Bug Fixes

* **runtime:** accept component_type alias on register/heartbeat/deregister ([696b56d](https://github.com/noetl/server/commit/696b56d6eb29195c6a1373889a3ad9601d761d22)), closes [noetl/ai-meta#53](https://github.com/noetl/ai-meta/issues/53) [noetl/ai-meta#53](https://github.com/noetl/ai-meta/issues/53) [noetl/ai-meta#53](https://github.com/noetl/ai-meta/issues/53) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.8.0](https://github.com/noetl/server/compare/v2.7.0...v2.8.0) (2026-06-03)

### Features

* **orchestrator:** defer end-step completion for parallel branches ([c906e64](https://github.com/noetl/server/commit/c906e64574930ae3f5d3448c34feb61ef25c924e)), closes [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [#32](https://github.com/noetl/server/issues/32) [#33](https://github.com/noetl/server/issues/33) [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.7.0](https://github.com/noetl/server/compare/v2.6.0...v2.7.0) (2026-06-03)

### Features

* **orchestrator:** step.loop iterator fan-out + state aggregation ([2b1ba32](https://github.com/noetl/server/commit/2b1ba32366429127e76d3c7bdafbc6c23d230eae)), closes [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.6.0](https://github.com/noetl/server/compare/v2.5.0...v2.6.0) (2026-06-03)

### Features

* **orchestrator:** wire step.when enable guard with skip chain ([7de832e](https://github.com/noetl/server/commit/7de832e6239ea1cc0e7dfb0bb946d0c73c57373d)), closes [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.5.0](https://github.com/noetl/server/compare/v2.4.3...v2.5.0) (2026-06-03)

### Features

* **orchestrator:** wire trigger_orchestrator + persist_engine_command ([af2d089](https://github.com/noetl/server/commit/af2d0891efb1bb9df02a23b875a301e141ea6de2)), closes [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#29](https://github.com/noetl/server/issues/29) [#49](https://github.com/noetl/server/issues/49) [noetl/server#22](https://github.com/noetl/server/issues/22) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.4.3](https://github.com/noetl/server/compare/v2.4.2...v2.4.3) (2026-06-03)

### Bug Fixes

* **events:** result envelope must match chk_event_result_shape ([aae4000](https://github.com/noetl/server/commit/aae400031c3bff02f089d33e6ac590919ca2ac08)), closes [noetl/server#29](https://github.com/noetl/server/issues/29) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/server#29](https://github.com/noetl/server/issues/29) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.4.2](https://github.com/noetl/server/compare/v2.4.1...v2.4.2) (2026-06-03)

### Bug Fixes

* **execute:** emit args:{} (not args:null) in command.issued context ([c2de98f](https://github.com/noetl/server/commit/c2de98f89eca397b725110f1d31f336361af9a50)), closes [noetl/server#27](https://github.com/noetl/server/issues/27) [noetl/server#27](https://github.com/noetl/server/issues/27) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.4.1](https://github.com/noetl/server/compare/v2.4.0...v2.4.1) (2026-06-03)

### Bug Fixes

* **execute:** publish command notification to NATS + insert command row ([1c71b8c](https://github.com/noetl/server/commit/1c71b8caa5cc9dcddecaf15abcbf04d3848aa28a)), closes [noetl/server#26](https://github.com/noetl/server/issues/26) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/server#26](https://github.com/noetl/server/issues/26) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.4.0](https://github.com/noetl/server/compare/v2.3.0...v2.4.0) (2026-06-03)

### Features

* **metrics:** instrument the other 5 Phase B write endpoints ([f024465](https://github.com/noetl/server/commit/f024465c6ba1af7323edc559b28bf8284ece572f)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.3.0](https://github.com/noetl/server/compare/v2.2.1...v2.3.0) (2026-06-03)

### Features

* **metrics:** prometheus surface + instrument POST /api/events ([fc4a33a](https://github.com/noetl/server/commit/fc4a33aecadb3fb61fd5f24dc13dfb249e667597)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#21](https://github.com/noetl/server/issues/21) [#21](https://github.com/noetl/server/issues/21) [noetl/server#21](https://github.com/noetl/server/issues/21) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.2.1](https://github.com/noetl/server/compare/v2.2.0...v2.2.1) (2026-06-03)

### Bug Fixes

* **catalog:** emit null for optional response fields (Python parity) ([3c7e2c3](https://github.com/noetl/server/commit/3c7e2c383a41effc6628b99b79a1886401a1841b)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.2.0](https://github.com/noetl/server/compare/v2.1.6...v2.2.0) (2026-06-03)

### Features

* **catalog:** port /api/catalog/{path}/ui_schema from Python ([52c1f3a](https://github.com/noetl/server/commit/52c1f3aa14d4097dbc9e80c8ff0d8a8481940d44)), closes [noetl/server#18](https://github.com/noetl/server/issues/18) [noetl/ops#152](https://github.com/noetl/ops/issues/152) [noetl/ops#152](https://github.com/noetl/ops/issues/152) [noetl/server#18](https://github.com/noetl/server/issues/18) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.6](https://github.com/noetl/server/compare/v2.1.5...v2.1.6) (2026-06-03)

### Bug Fixes

* **routes:** remove /api/runtimes route for Phase A parity ([c382787](https://github.com/noetl/server/commit/c382787957446b5b5fbe983ace61d36ecd8b3f6a)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [#2](https://github.com/noetl/server/issues/2) [noetl/server#18](https://github.com/noetl/server/issues/18) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.5](https://github.com/noetl/server/compare/v2.1.4...v2.1.5) (2026-06-03)

### Bug Fixes

* **schema:** align event_type literals + catalog column names with real DB ([dbd4e33](https://github.com/noetl/server/commit/dbd4e33acf0fcec5ad7e1531d965e3a49910c5d2)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#15](https://github.com/noetl/server/issues/15) [#16](https://github.com/noetl/server/issues/16) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.4](https://github.com/noetl/server/compare/v2.1.3...v2.1.4) (2026-06-03)

### Bug Fixes

* **execution:** noetl.event.created_at is TIMESTAMP, not TIMESTAMPTZ ([f450465](https://github.com/noetl/server/commit/f450465c9d16f7840fc5fcd873e2448103a5dd8d)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.3](https://github.com/noetl/server/compare/v2.1.2...v2.1.3) (2026-06-03)

### Bug Fixes

* **credential:** SQL column is data_encrypted not data ([feebf7d](https://github.com/noetl/server/commit/feebf7d3f6c9b617a503da633eaea665d5cd07ed)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [#49](https://github.com/noetl/server/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.2](https://github.com/noetl/server/compare/v2.1.1...v2.1.2) (2026-06-03)

### Bug Fixes

* **routes:** migrate path syntax to axum 0.8 (`:param` → `{param}`) ([30c6254](https://github.com/noetl/server/commit/30c625440f3e93934bf1cef630a841ff75c891fa)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.1](https://github.com/noetl/server/compare/v2.1.0...v2.1.1) (2026-06-02)

### Bug Fixes

* **internal-api:** project_events SQL matches actual noetl.event schema ([b2545b7](https://github.com/noetl/server/commit/b2545b7db34322ed510ac1b25cc421db50096823)), closes [noetl/noetl#660](https://github.com/noetl/noetl/issues/660) [noetl/ai-meta#46](https://github.com/noetl/ai-meta/issues/46) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.1.0](https://github.com/noetl/server/compare/v2.0.1...v2.1.0) (2026-06-02)

### Features

* **internal-api:** mirror /api/internal/* endpoints from Python noetl-server ([053e601](https://github.com/noetl/server/commit/053e60186f3f03a3048c51d8fe36ac0d8eb4cefa)), closes [noetl/noetl#659](https://github.com/noetl/noetl/issues/659) [noetl/ai-meta#46](https://github.com/noetl/ai-meta/issues/46) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#46](https://github.com/noetl/ai-meta/issues/46) [noetl/server#11](https://github.com/noetl/server/issues/11)

## [2.0.1](https://github.com/noetl/server/compare/v2.0.0...v2.0.1) (2026-05-31)

### Bug Fixes

* **ci:** trigger release-server after semantic-release tags a version + grant permissions ([315a755](https://github.com/noetl/server/commit/315a755dc50286a9ba47f016526054c2af9cbf51)), closes [#4](https://github.com/noetl/server/issues/4) [#5](https://github.com/noetl/server/issues/5) [#6](https://github.com/noetl/server/issues/6) [worker#4](https://github.com/noetl/worker/issues/4) [#5](https://github.com/noetl/server/issues/5) [noetl/ai-meta#30](https://github.com/noetl/ai-meta/issues/30)

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
