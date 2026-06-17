# Changelog

All notable changes to this project will be documented in this file.

## [3.12.0](https://github.com/noetl/server/compare/v3.11.0...v3.12.0) (2026-06-17)

### Features

* **objects:** object-store endpoint — the Feather tier (v3.12.0, [#105](https://github.com/noetl/server/issues/105) Round 5) ([#212](https://github.com/noetl/server/issues/212)) ([a08c5d4](https://github.com/noetl/server/commit/a08c5d4e81f9a9e12732857445eb97402d9b88fb)), closes [noetl/server#211](https://github.com/noetl/server/issues/211)

## [3.11.0](https://github.com/noetl/server/compare/v3.10.0...v3.11.0) (2026-06-17)

### Features

* **plugins:** plug-in module registry endpoints (v3.11.0) ([#210](https://github.com/noetl/server/issues/210)) ([62a5727](https://github.com/noetl/server/commit/62a5727f758d4da2b1d0f14153e275cd7b820511)), closes [noetl/ai-meta#101](https://github.com/noetl/ai-meta/issues/101) [worker#91](https://github.com/noetl/worker/issues/91) [noetl/ai-meta#101](https://github.com/noetl/ai-meta/issues/101) [noetl/ai-meta#101](https://github.com/noetl/ai-meta/issues/101) [noetl/ai-meta#101](https://github.com/noetl/ai-meta/issues/101) [#105](https://github.com/noetl/server/issues/105) [noetl/ai-meta#105](https://github.com/noetl/ai-meta/issues/105) [noetl/ai-meta#105](https://github.com/noetl/ai-meta/issues/105)

## [3.10.0](https://github.com/noetl/server/compare/v3.9.0...v3.10.0) (2026-06-16)

### Features

* **orchestrator:** batch cursor-fanout command dispatch (multi-row INSERT) ([#198](https://github.com/noetl/server/issues/198)) ([5a1f7c6](https://github.com/noetl/server/commit/5a1f7c6f36f77d7a233039c74263c6489efb9c83)), closes [noetl/ai-meta#102](https://github.com/noetl/ai-meta/issues/102) [noetl/ai-meta#102](https://github.com/noetl/ai-meta/issues/102)

## [3.9.0](https://github.com/noetl/server/compare/v3.8.0...v3.9.0) (2026-06-16)

### Features

* **orchestrator:** bounded-memory state + results-by-reference + stall-proof reconcile ([#101](https://github.com/noetl/server/issues/101)) ([#197](https://github.com/noetl/server/issues/197)) ([ee77092](https://github.com/noetl/server/commit/ee77092c090fa18d5b36c54c068880244e6677b0))

## [3.8.0](https://github.com/noetl/server/compare/v3.7.1...v3.8.0) (2026-06-15)

### Features

* **orchestrator:** cursor/claim loop mode (loop.cursor + mode: cursor) ([#196](https://github.com/noetl/server/issues/196)) ([3568c61](https://github.com/noetl/server/commit/3568c615e0b4fb1c05b4d494abc07e5c34c371b1)), closes [noetl/ai-meta#100](https://github.com/noetl/ai-meta/issues/100) [noetl/ai-meta#100](https://github.com/noetl/ai-meta/issues/100) [noetl/ai-meta#100](https://github.com/noetl/ai-meta/issues/100) [noetl/ai-meta#100](https://github.com/noetl/ai-meta/issues/100) [noetl/ai-meta#100](https://github.com/noetl/ai-meta/issues/100)

## [3.7.1](https://github.com/noetl/server/compare/v3.7.0...v3.7.1) (2026-06-14)

### Bug Fixes

* **internal:** cleanup tolerates un-droppable event partitions ([#195](https://github.com/noetl/server/issues/195)) ([b2c9b45](https://github.com/noetl/server/commit/b2c9b45506281a758c8693ef9f8596fd0f4b07d1)), closes [noetl/ai-meta#96](https://github.com/noetl/ai-meta/issues/96)

## [3.7.0](https://github.com/noetl/server/compare/v3.6.0...v3.7.0) (2026-06-14)

### Features

* **internal:** event retention drops old partitions instead of DELETE ([#194](https://github.com/noetl/server/issues/194)) ([8999eb9](https://github.com/noetl/server/commit/8999eb9fbe86df3b685a3cbf2b5f551a3658f140)), closes [noetl/ai-meta#96](https://github.com/noetl/ai-meta/issues/96) [noetl/ai-meta#96](https://github.com/noetl/ai-meta/issues/96)

## [3.6.0](https://github.com/noetl/server/compare/v3.5.3...v3.6.0) (2026-06-14)

### Features

* **internal:** add POST /api/internal/cleanup/purge for scheduled retention ([#193](https://github.com/noetl/server/issues/193)) ([77231de](https://github.com/noetl/server/commit/77231deda1d9cda5b802f19cca6410a9f8d749a9)), closes [noetl/ai-meta#96](https://github.com/noetl/ai-meta/issues/96) [noetl/ai-meta#96](https://github.com/noetl/ai-meta/issues/96)

## [3.5.3](https://github.com/noetl/server/compare/v3.5.2...v3.5.3) (2026-06-14)

### Bug Fixes

* Python-compatible truthiness in evaluate_condition (auth playbook stall) ([#192](https://github.com/noetl/server/issues/192)) ([b99d8e4](https://github.com/noetl/server/commit/b99d8e47280af067755103a06b828cbb205c0613)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [3.5.2](https://github.com/noetl/server/compare/v3.5.1...v3.5.2) (2026-06-14)

### Bug Fixes

* env-gated sqlx statement-cache capacity for transaction-mode poolers ([#191](https://github.com/noetl/server/issues/191)) ([0577cc6](https://github.com/noetl/server/commit/0577cc66ef8601b9710799d5d430a7fa60c412d7)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [3.5.1](https://github.com/noetl/server/compare/v3.5.0...v3.5.1) (2026-06-12)

### Bug Fixes

* pin time =0.3.47 to dodge async-nats 0.38 E0119 build break ([#190](https://github.com/noetl/server/issues/190)) ([55d2dfc](https://github.com/noetl/server/commit/55d2dfc8f9e7e5f02ff66be22e857e4132ee2a34)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [3.5.0](https://github.com/noetl/server/compare/v3.4.2...v3.5.0) (2026-06-12)

### Features

* batch execute + opt-in dedup window (subscription scale hardening) ([#189](https://github.com/noetl/server/issues/189)) ([1c4b88a](https://github.com/noetl/server/commit/1c4b88a2425551e6b3fe9f8b1d48c2f8a34f9e2a)), closes [noetl/ai-meta#90](https://github.com/noetl/ai-meta/issues/90) [noetl/server#188](https://github.com/noetl/server/issues/188)

## [3.4.2](https://github.com/noetl/server/compare/v3.4.1...v3.4.2) (2026-06-12)

### Bug Fixes

* **catalog:** gcs/s3 spool credential optional (ADC / Workload Identity) ([c2ba6da](https://github.com/noetl/server/commit/c2ba6dadb74c15dd55dc3130d6e1e5d14641091b)), closes [noetl/server#186](https://github.com/noetl/server/issues/186) [noetl/ai-meta#90](https://github.com/noetl/ai-meta/issues/90)

## [3.4.1](https://github.com/noetl/server/compare/v3.4.0...v3.4.1) (2026-06-12)

### Bug Fixes

* subscription lifecycle status must ignore spool/circuit/message events ([#90](https://github.com/noetl/server/issues/90) Phase 4) ([#185](https://github.com/noetl/server/issues/185)) ([5611a9a](https://github.com/noetl/server/commit/5611a9a48c55bbf6c5db8630571f6cac5e205b8d))

## [3.4.0](https://github.com/noetl/server/compare/v3.3.0...v3.4.0) (2026-06-12)

### Features

* validate kind:Subscription spool config block ([#90](https://github.com/noetl/server/issues/90) Phase 4) ([#184](https://github.com/noetl/server/issues/184)) ([1f49807](https://github.com/noetl/server/commit/1f49807c8c0b5a0a760806ad7f346b0e50d4de2e))

## [3.3.0](https://github.com/noetl/server/compare/v3.2.0...v3.3.0) (2026-06-12)

### Features

* push-ingress config endpoint + push catalog validation ([#90](https://github.com/noetl/server/issues/90) Phase 3) ([#182](https://github.com/noetl/server/issues/182)) ([7f62537](https://github.com/noetl/server/commit/7f62537e2b8975d373682ec2dcc67f8dc14ece37)), closes [noetl/server#181](https://github.com/noetl/server/issues/181)

## [3.2.0](https://github.com/noetl/server/compare/v3.1.0...v3.2.0) (2026-06-12)

### Features

* kind:Subscription type + lifecycle + pool routing + W3C trace ([#90](https://github.com/noetl/server/issues/90) Phase 2) ([#180](https://github.com/noetl/server/issues/180)) ([0e435b8](https://github.com/noetl/server/commit/0e435b808aab9abea5468aa770b1a7d3ca9ffdae))

## [3.1.0](https://github.com/noetl/server/compare/v3.0.6...v3.1.0) (2026-06-11)

### Features

* **playbook:** accept `subscription` tool kind in ToolKind validation ([#178](https://github.com/noetl/server/issues/178)) ([d3126f8](https://github.com/noetl/server/commit/d3126f8301598f81f3555c5afa93d6f237291ba2)), closes [noetl/ai-meta#90](https://github.com/noetl/ai-meta/issues/90) [noetl/ai-meta#90](https://github.com/noetl/ai-meta/issues/90)

## [3.0.6](https://github.com/noetl/server/compare/v3.0.5...v3.0.6) (2026-06-11)

### Bug Fixes

* **template:** round-trip JSON null in whole-object {{ step }} references ([#177](https://github.com/noetl/server/issues/177)) ([d91d26f](https://github.com/noetl/server/commit/d91d26f6de70985a518b06b5a5627828a28559ab)), closes [noetl/ai-meta#89](https://github.com/noetl/ai-meta/issues/89)

## [3.0.5](https://github.com/noetl/server/compare/v3.0.4...v3.0.5) (2026-06-11)

### Bug Fixes

* **orchestrator:** durable loop ctx propagation + loop-exit hang ([#85](https://github.com/noetl/server/issues/85)) ([#176](https://github.com/noetl/server/issues/176)) ([1ce7f2b](https://github.com/noetl/server/commit/1ce7f2b012765d1bc600cd162e6edad493ddf4e1)), closes [#83](https://github.com/noetl/server/issues/83) [#84](https://github.com/noetl/server/issues/84)

## [3.0.4](https://github.com/noetl/server/compare/v3.0.3...v3.0.4) (2026-06-10)

### Bug Fixes

* **orchestrator:** unblock workflow loops + loop.done-gated transitions ([#175](https://github.com/noetl/server/issues/175)) ([1a92a81](https://github.com/noetl/server/commit/1a92a81b5be63f872310658a14effd81bce0d3bb))

## [3.0.3](https://github.com/noetl/server/compare/v3.0.2...v3.0.3) (2026-06-10)

### Bug Fixes

* **container-callback:** insert call.done with deployed event schema ([#173](https://github.com/noetl/server/issues/173)) ([1b920a7](https://github.com/noetl/server/commit/1b920a7b9661040e0d1543de9e5a157fe9ca37da)), closes [noetl/ai-meta#43](https://github.com/noetl/ai-meta/issues/43) [noetl/ai-meta#43](https://github.com/noetl/ai-meta/issues/43)

## [3.0.2](https://github.com/noetl/server/compare/v3.0.1...v3.0.2) (2026-06-10)

### Bug Fixes

* accept array command for container tool (ToolSpec.command -> Value) ([#172](https://github.com/noetl/server/issues/172)) ([7284fed](https://github.com/noetl/server/commit/7284fed96d949e40b69a8f1b10d86d81107e0b31)), closes [noetl/ai-meta#81](https://github.com/noetl/ai-meta/issues/81)

## [3.0.1](https://github.com/noetl/server/compare/v3.0.0...v3.0.1) (2026-06-10)

### Bug Fixes

* extract _context_updates from task_sequence result for cross-step propagation ([88a58e1](https://github.com/noetl/server/commit/88a58e1d9032e9ce5d4de3bc149c6de16e71c2c9))
* lowercase catalog kind on register to match resource FK ([f0301e8](https://github.com/noetl/server/commit/f0301e8654b0d1c4cef699549772f33d04dc896f))
* persist ctx/workload namespace shims on Command for worker-side pipeline input rendering ([ee8dd67](https://github.com/noetl/server/commit/ee8dd67d74567aff675b29c43f2e4be040b5b86e)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)
* preserve spec block unrendered in render_pipeline_config ([d4a8048](https://github.com/noetl/server/commit/d4a804857f9b73937fd5b4650e47cf13dc6cb949))
* raise result store body limit to 64MB + preserve command/spec in pipeline render ([f348922](https://github.com/noetl/server/commit/f3489228e9b6210228a10df89096c36362f9fd56)), closes [noetl/ai-meta#69](https://github.com/noetl/ai-meta/issues/69) [noetl/ai-meta#69](https://github.com/noetl/ai-meta/issues/69)

## [3.0.0](https://github.com/noetl/server/compare/v2.63.0...v3.0.0) (2026-06-09)

### ⚠ BREAKING CHANGES

* replace generic render_value_deferring with targeted pipeline rendering

### Features

* replace generic render_value_deferring with targeted pipeline rendering ([02dc266](https://github.com/noetl/server/commit/02dc26627bf547f283cd18d21694c79ec9406156)), closes [noetl/ai-meta#77](https://github.com/noetl/ai-meta/issues/77)

## [2.63.0](https://github.com/noetl/server/compare/v2.62.1...v2.63.0) (2026-06-09)

### Features

* add step-level set: mutation support to orchestrator ([48cb008](https://github.com/noetl/server/commit/48cb00872c6a5f426587dd046b49f21a8888a193))

### Bug Fixes

* add ctx/workload namespace shims to orchestrator evaluation paths ([b05f978](https://github.com/noetl/server/commit/b05f9785437f1d9ab65b015da42f2408d556e3c6))

## [2.62.1](https://github.com/noetl/server/compare/v2.62.0...v2.62.1) (2026-06-09)

### Bug Fixes

* resolve all pre-existing clippy warnings under -D warnings ([53f93f5](https://github.com/noetl/server/commit/53f93f515d1e25a508500f3ecd41f9fc1b2a0bf1)), closes [#161](https://github.com/noetl/server/issues/161)

## [2.62.0](https://github.com/noetl/server/compare/v2.61.1...v2.62.0) (2026-06-09)

### Features

* sequential-mode iterator dispatch ([#76](https://github.com/noetl/server/issues/76)) ([9bfd4e3](https://github.com/noetl/server/commit/9bfd4e32d3ea3561ba1aa23dc0ed46fcc2ac14ac))

## [2.61.1](https://github.com/noetl/server/compare/v2.61.0...v2.61.1) (2026-06-08)

### Bug Fixes

* **status:** honest in-flight check prevents premature COMPLETED verdict ([4c14750](https://github.com/noetl/server/commit/4c1475038896f9b47420f8f255563376c92b129c)), closes [noetl/ai-meta#72](https://github.com/noetl/ai-meta/issues/72)

## [2.61.0](https://github.com/noetl/server/compare/v2.60.0...v2.61.0) (2026-06-08)

### Features

* **engine:** expose ctx + workload namespaces in dispatch render context ([f554141](https://github.com/noetl/server/commit/f5541415d6cb80077a9943d7870bb0ce821aca8e)), closes [noetl/ai-meta#74](https://github.com/noetl/ai-meta/issues/74)

## [2.60.0](https://github.com/noetl/server/compare/v2.59.0...v2.60.0) (2026-06-08)

### Features

* **engine:** propagate arc-level set: mutations into downstream step context ([e413bef](https://github.com/noetl/server/commit/e413bef5a4a0750e9163fd26ca02c340e7ff0323)), closes [noetl/ai-meta#73](https://github.com/noetl/ai-meta/issues/73)

## [2.59.0](https://github.com/noetl/server/compare/v2.58.0...v2.59.0) (2026-06-08)

### Features

* **engine:** fan out start step when it has a loop block ([33a2751](https://github.com/noetl/server/commit/33a275167f7f843e8b821f130f92a84f337e8e6d)), closes [noetl/ai-meta#73](https://github.com/noetl/ai-meta/issues/73) [noetl/server#161](https://github.com/noetl/server/issues/161) [noetl/ai-meta#73](https://github.com/noetl/ai-meta/issues/73)

## [2.58.0](https://github.com/noetl/server/compare/v2.57.2...v2.58.0) (2026-06-08)

### Features

* **api:** port PUT /api/result/<eid> + GET /api/result/resolve from Python ([0c0d13b](https://github.com/noetl/server/commit/0c0d13b490533fec8665b7d23f8508b64a1aabe7)), closes [noetl/ai-meta#70](https://github.com/noetl/ai-meta/issues/70)

## [2.57.2](https://github.com/noetl/server/compare/v2.57.1...v2.57.2) (2026-06-08)

### Bug Fixes

* **orchestrator:** emit step.skipped for untaken exclusive-routing siblings ([5855b8e](https://github.com/noetl/server/commit/5855b8e829bcaf675280967d86ba0d018a96ff35)), closes [noetl/ai-meta#67](https://github.com/noetl/ai-meta/issues/67) [noetl/ai-meta#67](https://github.com/noetl/ai-meta/issues/67)

## [2.57.1](https://github.com/noetl/server/compare/v2.57.0...v2.57.1) (2026-06-07)

### Bug Fixes

* **orchestrator:** expose `step.data` accessor on user_data shaped step results ([3fe4e76](https://github.com/noetl/server/commit/3fe4e7686aa121d815eae6056c8d1af325bef188)), closes [noetl/ai-meta#65](https://github.com/noetl/ai-meta/issues/65) [#65](https://github.com/noetl/server/issues/65) [noetl/ai-meta#66](https://github.com/noetl/ai-meta/issues/66)

## [2.57.0](https://github.com/noetl/server/compare/v2.56.0...v2.57.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R7 — cross-server parity harness ([#148](https://github.com/noetl/server/issues/148)) ([f90be57](https://github.com/noetl/server/commit/f90be57761cd74b03ae2e6974325410ffc0bac95)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.56.0](https://github.com/noetl/server/compare/v2.55.0...v2.56.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R6 — payload resolver ([#148](https://github.com/noetl/server/issues/148)) ([7e80cf5](https://github.com/noetl/server/commit/7e80cf5c78f83b061c86b3e43be0d8a056b83a36)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.55.0](https://github.com/noetl/server/compare/v2.54.0...v2.55.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R5 — snapshot seed + base_state + upcaster digest ([#148](https://github.com/noetl/server/issues/148)) ([e42ccba](https://github.com/noetl/server/commit/e42ccba5149d1125478a6ac42439e5813ea11983)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.54.0](https://github.com/noetl/server/compare/v2.53.0...v2.54.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R4 — typed Checksum + projection_checksums ([#148](https://github.com/noetl/server/issues/148)) ([a22161c](https://github.com/noetl/server/commit/a22161c413b069b7a760c027c4dbea4be222e19e)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.53.0](https://github.com/noetl/server/compare/v2.52.0...v2.53.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R3 — loops + business_objects projections ([#148](https://github.com/noetl/server/issues/148)) ([5211ff0](https://github.com/noetl/server/commit/5211ff06faef970044f33bcfea5112bed80a5939)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.52.0](https://github.com/noetl/server/compare/v2.51.1...v2.52.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R2 — stages + frames + commands projections ([#148](https://github.com/noetl/server/issues/148)) ([43f3a08](https://github.com/noetl/server/commit/43f3a08b559d696417c20bb9a31f6c899b4c3fc5)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.51.1](https://github.com/noetl/server/compare/v2.51.0...v2.51.1) (2026-06-07)

### Bug Fixes

* **main:** use ReplayService import-statement shape (release-build fix, [#150](https://github.com/noetl/server/issues/150)) ([0e24b55](https://github.com/noetl/server/commit/0e24b55748e8af8f174f95552cd1d57115e87d76)), closes [server#149](https://github.com/noetl/server/issues/149) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/server#148](https://github.com/noetl/server/issues/148) [noetl/server#149](https://github.com/noetl/server/issues/149)
* **replay:** coerce noetl.event.created_at TIMESTAMP → TIMESTAMPTZ in load_events ([#150](https://github.com/noetl/server/issues/150)) ([585fa1e](https://github.com/noetl/server/commit/585fa1eaf32a89d8db9d93dc0251eebd234e6f18)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.51.0](https://github.com/noetl/server/compare/v2.50.1...v2.51.0) (2026-06-07)

### Features

* **replay:** Phase D R5 R1 — endpoint scaffold + execution projection ([#148](https://github.com/noetl/server/issues/148)) ([b85e17a](https://github.com/noetl/server/commit/b85e17abba4677fc15953e0b5b829513bc01073b)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.50.1](https://github.com/noetl/server/compare/v2.50.0...v2.50.1) (2026-06-07)

### Bug Fixes

* **executions:** status endpoint short-circuits on terminal events ([#146](https://github.com/noetl/server/issues/146)) ([f026611](https://github.com/noetl/server/commit/f02661141dcd2027449268ec2e0f1e1c647c6bfc)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.50.0](https://github.com/noetl/server/compare/v2.49.0...v2.50.0) (2026-06-07)

### Features

* **engine:** apply_event handles step.skipped — closes barrier follow-up ([#144](https://github.com/noetl/server/issues/144)) ([ed7742a](https://github.com/noetl/server/commit/ed7742a61a832a082419bb38fc8163d0e0e8695e)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [server#143](https://github.com/noetl/server/issues/143) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.49.0](https://github.com/noetl/server/compare/v2.48.0...v2.49.0) (2026-06-07)

### Features

* **engine:** fan-in / reduce barrier — defer multi-upstream dispatch ([#142](https://github.com/noetl/server/issues/142)) ([8e7a5de](https://github.com/noetl/server/commit/8e7a5de73e0a41c2387545527f665b6fd2e02875)), closes [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49) [noetl/ai-meta#49](https://github.com/noetl/ai-meta/issues/49)

## [2.48.0](https://github.com/noetl/server/compare/v2.47.0...v2.48.0) (2026-06-07)

### Features

* **internal:** container-callback endpoint (Round 2 of [#43](https://github.com/noetl/server/issues/43)) ([3a05cc4](https://github.com/noetl/server/commit/3a05cc4389e282acf73a14429a2e93faafe06179)), closes [noetl/ops#166](https://github.com/noetl/ops/issues/166) [noetl/ops#166](https://github.com/noetl/ops/issues/166) [noetl/tools#36](https://github.com/noetl/tools/issues/36) [#140](https://github.com/noetl/server/issues/140)

## [2.47.0](https://github.com/noetl/server/compare/v2.46.0...v2.47.0) (2026-06-07)

### Features

* **secrets:** GCP iamcredentials.generateAccessToken provider (Phase 6d.2) ([55636ca](https://github.com/noetl/server/commit/55636ca13b22f60fc3d02671e8204022706edd6e)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [#133](https://github.com/noetl/server/issues/133) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.46.0](https://github.com/noetl/server/compare/v2.45.0...v2.46.0) (2026-06-07)

### Features

* **secrets:** Azure AAD client-credentials provider (Phase 6d.3) ([950938f](https://github.com/noetl/server/commit/950938f39fb33d2bb949d8997f5cc1dca1621293)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [#134](https://github.com/noetl/server/issues/134) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.45.0](https://github.com/noetl/server/compare/v2.44.0...v2.45.0) (2026-06-07)

### Features

* **secrets:** AWS STS AssumeRoleWithWebIdentity provider (Phase 6d.1) ([7a02f05](https://github.com/noetl/server/commit/7a02f0526600e3ab28bd967bca3e59e77bc47821)), closes [#132](https://github.com/noetl/server/issues/132) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.44.0](https://github.com/noetl/server/compare/v2.43.0...v2.44.0) (2026-06-07)

### Features

* **keychain:** background refresh + stampede collapse (Phase 7c.3) ([da4bd37](https://github.com/noetl/server/commit/da4bd37013781cc5c4aec10b83133a3e1f313e6c)), closes [server#125](https://github.com/noetl/server/issues/125) [server#131](https://github.com/noetl/server/issues/131) [#135](https://github.com/noetl/server/issues/135) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.43.0](https://github.com/noetl/server/compare/v2.42.0...v2.43.0) (2026-06-07)

### Features

* **audit:** noetl.secret_audit table + DbAuditSink + query endpoint (Phase 7b.2) ([73dfcc5](https://github.com/noetl/server/commit/73dfcc589c54c502b9b4426bae74bfb58b3768de)), closes [#128](https://github.com/noetl/server/issues/128) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)
* **keychain:** should_refresh primitive (Phase 7c.2) ([d39e9f1](https://github.com/noetl/server/commit/d39e9f18169ac238625c1dd75c472170753e64fd)), closes [#130](https://github.com/noetl/server/issues/130) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.42.0](https://github.com/noetl/server/compare/v2.41.0...v2.42.0) (2026-06-06)

### Features

* **wallet:** KEK rotation endpoint + DB scans + key-status (Phase 7a.2) ([d6b8723](https://github.com/noetl/server/commit/d6b872351c900bff257392438ac072246c102c6c)), closes [#126](https://github.com/noetl/server/issues/126) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.41.0](https://github.com/noetl/server/compare/v2.40.0...v2.41.0) (2026-06-06)

### Features

* **secrets:** token auto-renewal primitives (Phase 7c) ([f51220a](https://github.com/noetl/server/commit/f51220a1ca6b3658e088a76827f601f05941758d)), closes [#124](https://github.com/noetl/server/issues/124) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.40.0](https://github.com/noetl/server/compare/v2.39.0...v2.40.0) (2026-06-06)

### Features

* **services:** secret-resolution audit service (Phase 7b primitives) ([24d572f](https://github.com/noetl/server/commit/24d572f48395a7976af68b21687a509d7cee04fa)), closes [#122](https://github.com/noetl/server/issues/122) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.39.0](https://github.com/noetl/server/compare/v2.38.0...v2.39.0) (2026-06-06)

### Features

* **crypto:** wallet KEK rotation primitives (Phase 7a) ([773e188](https://github.com/noetl/server/commit/773e188d415a99d9fa828b443b609c12d7eb87eb)), closes [#120](https://github.com/noetl/server/issues/120) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.38.0](https://github.com/noetl/server/compare/v2.37.0...v2.38.0) (2026-06-06)

### Features

* **secrets:** cross-region broker (Phase 6e — closes Phase 6) ([19b58b9](https://github.com/noetl/server/commit/19b58b949b9b5d74d749059c6a84250faba2a81c)), closes [#118](https://github.com/noetl/server/issues/118) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.37.0](https://github.com/noetl/server/compare/v2.36.0...v2.37.0) (2026-06-06)

### Features

* **secrets:** dynamic-secret primitives + cache honors issuer TTL (Phase 6d) ([99a6be6](https://github.com/noetl/server/commit/99a6be6c468fb64ab8eef09055c7fb3e43b2d3ea)), closes [#116](https://github.com/noetl/server/issues/116) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.36.0](https://github.com/noetl/server/compare/v2.35.0...v2.36.0) (2026-06-06)

### Features

* **secrets:** residency-policy gate (Phase 6c) ([0f4bc14](https://github.com/noetl/server/commit/0f4bc14574a110c0818a39a693a3d4de9abb9da9)), closes [#114](https://github.com/noetl/server/issues/114) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.35.0](https://github.com/noetl/server/compare/v2.34.0...v2.35.0) (2026-06-06)

### Features

* **secrets:** ProviderRegistry + per-(provider, region) metrics (Phase 6b) ([d86a32b](https://github.com/noetl/server/commit/d86a32b018f22d65e16d6001d4d7ab8a3c63b977)), closes [server#111](https://github.com/noetl/server/issues/111) [#112](https://github.com/noetl/server/issues/112) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.34.0](https://github.com/noetl/server/compare/v2.33.0...v2.34.0) (2026-06-06)

### Features

* **secrets:** region tag on keychain entries + per-region routing (Phase 6a) ([154b73b](https://github.com/noetl/server/commit/154b73b925767d84982d2db56df3857c9950909d)), closes [#110](https://github.com/noetl/server/issues/110) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.33.0](https://github.com/noetl/server/compare/v2.32.0...v2.33.0) (2026-06-06)

### Features

* **api:** sealed credential delivery endpoint (Secrets Wallet Phase 5b) ([68fc193](https://github.com/noetl/server/commit/68fc193936c449034bf74a341af8f3e6390efa63)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [#108](https://github.com/noetl/server/issues/108) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.32.0](https://github.com/noetl/server/compare/v2.31.0...v2.32.0) (2026-06-06)

### Features

* **crypto:** sealed payload primitives (Secrets Wallet Phase 5a) ([b551471](https://github.com/noetl/server/commit/b551471a1700174b86d8ffbb15a5cac1255d7843)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [#106](https://github.com/noetl/server/issues/106) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.31.0](https://github.com/noetl/server/compare/v2.30.0...v2.31.0) (2026-06-06)

### Features

* **secrets:** AWS Secrets Manager + Azure Key Vault providers ([348534d](https://github.com/noetl/server/commit/348534dac1a8a6bf9e688f5aa526bf2513c09341)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [server#97](https://github.com/noetl/server/issues/97) [server#101](https://github.com/noetl/server/issues/101) [#104](https://github.com/noetl/server/issues/104) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.30.0](https://github.com/noetl/server/compare/v2.29.0...v2.30.0) (2026-06-06)

### Features

* **tls:** opt-in TLS/mTLS listener for the control-plane API ([85a805d](https://github.com/noetl/server/commit/85a805d914c599329d9d16fac3319d47ea977d50)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [#102](https://github.com/noetl/server/issues/102) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.29.0](https://github.com/noetl/server/compare/v2.28.1...v2.29.0) (2026-06-06)

### Features

* **secrets:** add HashiCorp Vault (KV v2) provider behind SecretProvider ([bce9ab1](https://github.com/noetl/server/commit/bce9ab142f44d3429d5fc55752d2b805a26fd410)), closes [#100](https://github.com/noetl/server/issues/100) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.28.1](https://github.com/noetl/server/compare/v2.28.0...v2.28.1) (2026-06-06)

### Performance Improvements

* **executions:** candidate-first list query + fix status aggregation ([d04fc94](https://github.com/noetl/server/commit/d04fc94e8188a6f2aed674c5ca28ed582206f06e)), closes [noetl/ai-meta#62](https://github.com/noetl/ai-meta/issues/62) [#98](https://github.com/noetl/server/issues/98) [noetl/ai-meta#62](https://github.com/noetl/ai-meta/issues/62)

## [2.28.0](https://github.com/noetl/server/compare/v2.27.2...v2.28.0) (2026-06-06)

### Features

* **secrets:** add Kubernetes Secrets provider behind SecretProvider ([a2e2b35](https://github.com/noetl/server/commit/a2e2b35b48ff8d3e404cf8f09cce6631498943f3)), closes [#96](https://github.com/noetl/server/issues/96) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.27.2](https://github.com/noetl/server/compare/v2.27.1...v2.27.2) (2026-06-06)

### Bug Fixes

* **orchestrator:** emit terminal playbook.failed on deterministic evaluate error ([27942ce](https://github.com/noetl/server/commit/27942cef5950eb3432201d3fa1ad7c84ed9075ff)), closes [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54) [#94](https://github.com/noetl/server/issues/94) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)

## [2.27.1](https://github.com/noetl/server/compare/v2.27.0...v2.27.1) (2026-06-06)

### Bug Fixes

* **parser:** order NextSpec variants so the list form doesn't deserialize as Router ([9c0a947](https://github.com/noetl/server/commit/9c0a94782acac06a6ee32fcad586d8b7924c9924))

## [2.27.0](https://github.com/noetl/server/compare/v2.26.0...v2.27.0) (2026-06-06)

### Features

* **secrets:** execution-scoped cache for resolved keychain secrets (Phase 3c) ([843ed8d](https://github.com/noetl/server/commit/843ed8dd2592f97bdf2e9af9e4fbed6d5e60635f)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.26.0](https://github.com/noetl/server/compare/v2.25.0...v2.26.0) (2026-06-06)

### Features

* **secrets:** resolve provider-backed keychain aliases on credential miss (Phase 3b R3b) ([4a08e48](https://github.com/noetl/server/commit/4a08e48bb750d594e9c6cb6d55fa46f35aafa29a)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.25.0](https://github.com/noetl/server/compare/v2.24.0...v2.25.0) (2026-06-06)

### Features

* **secrets:** keychain secret-source resolver logic + provider factory (Phase 3b R3a) ([318429b](https://github.com/noetl/server/commit/318429b635522da399e097664930b01c2e8a2701)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.24.0](https://github.com/noetl/server/compare/v2.23.0...v2.24.0) (2026-06-06)

### Features

* **secrets:** model keychain secret-source defs + Playbook::find_keychain (Phase 3b R2) ([bdffcd3](https://github.com/noetl/server/commit/bdffcd388a6ae381dab2e75fe60fa89fde3eea66)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.23.0](https://github.com/noetl/server/compare/v2.22.0...v2.23.0) (2026-06-06)

### Features

* **secrets:** server-side GCP Secret Manager client (Phase 3b R1) ([588f367](https://github.com/noetl/server/commit/588f367cdb158be18c86d6d898b09e0831ecc48f)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.22.0](https://github.com/noetl/server/compare/v2.21.0...v2.22.0) (2026-06-05)

### Features

* **crypto:** GCP Cloud KMS KeyManager + runtime provider selection ([b6b5104](https://github.com/noetl/server/commit/b6b51047aefa27de5f91ac2c783e516b3d453d5e)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/server#80](https://github.com/noetl/server/issues/80) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.21.0](https://github.com/noetl/server/compare/v2.20.0...v2.21.0) (2026-06-05)

### Features

* **crypto:** envelope-encrypt credential + keychain storage (forward-only) ([b089036](https://github.com/noetl/server/commit/b089036f4a8946705bbecb839a42a6972a6174b1)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/server#78](https://github.com/noetl/server/issues/78) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.20.0](https://github.com/noetl/server/compare/v2.19.8...v2.20.0) (2026-06-05)

### Features

* **crypto:** envelope-encryption core (KeyManager + LocalDevKms + EnvelopeCipher) ([5539573](https://github.com/noetl/server/commit/5539573e92c10ddf282ebc7cd5228fb50b9056d8)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/server#76](https://github.com/noetl/server/issues/76) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.19.8](https://github.com/noetl/server/compare/v2.19.7...v2.19.8) (2026-06-05)

### Bug Fixes

* **crypto:** remove all-zeros default encryption key, fail closed ([48b4a6f](https://github.com/noetl/server/commit/48b4a6f82b604cb02ec040bf5f06fc013fbb3b28)), closes [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61) [noetl/server#74](https://github.com/noetl/server/issues/74) [noetl/ai-meta#61](https://github.com/noetl/ai-meta/issues/61)

## [2.19.7](https://github.com/noetl/server/compare/v2.19.6...v2.19.7) (2026-06-05)

### Bug Fixes

* **template:** defer task_sequence _prev/_results refs at command build ([15fd689](https://github.com/noetl/server/commit/15fd689b3c73eff0c7672dda4d17677152dd3ec6)), closes [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54) [noetl/server#72](https://github.com/noetl/server/issues/72) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)

## [2.19.6](https://github.com/noetl/server/compare/v2.19.5...v2.19.6) (2026-06-05)

### Bug Fixes

* **credential:** base64-armor encrypted data for TEXT data_encrypted column ([fe3f572](https://github.com/noetl/server/commit/fe3f572ec1a59acdaba2a227bbcf550c3e5615d3)), closes [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54) [noetl/server#70](https://github.com/noetl/server/issues/70) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)

## [2.19.5](https://github.com/noetl/server/compare/v2.19.4...v2.19.5) (2026-06-05)

### Bug Fixes

* **catalog:** cast smallint+1 back to smallint to avoid INT4 promotion ([52d8ca9](https://github.com/noetl/server/commit/52d8ca9c4de282b0c1e2bf00cd205626b25aae20)), closes [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54) [noetl/server#68](https://github.com/noetl/server/issues/68) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)
* **catalog:** insert_catalog_entry RETURNING uses catalog_id, not id ([0763c81](https://github.com/noetl/server/commit/0763c81e4be7e15b6fe641c0a5c65acd5198f315)), closes [noetl/server#68](https://github.com/noetl/server/issues/68) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)
* **events:** trigger orchestrator on end step's command.completed ([a50d718](https://github.com/noetl/server/commit/a50d7186eeaea11d2a83c94f6e8bd23476293b70)), closes [noetl/ai-meta#58](https://github.com/noetl/ai-meta/issues/58) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54) [noetl/server#68](https://github.com/noetl/server/issues/68) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)
* **orchestrator:** end step with action runs + task_sequence data flatten ([e1e71ee](https://github.com/noetl/server/commit/e1e71ee19e70045dfbd999f0ab3fd71bebdd822a)), closes [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54) [noetl/server#68](https://github.com/noetl/server/issues/68) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)
* **playbook:** ToolSpec Option fields skip serialize when None ([d673be0](https://github.com/noetl/server/commit/d673be0bcb9015d1dcafcabf9cab713b2eef6e95)), closes [noetl/server#68](https://github.com/noetl/server/issues/68) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)
* **template:** use Chainable undefined behavior + add e2e flatten test ([d9cebe3](https://github.com/noetl/server/commit/d9cebe36efe2dd351f9685ef4a63a4a69193aa2b)), closes [noetl/server#68](https://github.com/noetl/server/issues/68) [noetl/ai-meta#54](https://github.com/noetl/ai-meta/issues/54)

## [2.19.4](https://github.com/noetl/server/compare/v2.19.3...v2.19.4) (2026-06-05)

### Bug Fixes

* **orchestrator:** expose step data at top level + capture call.done ([0e23091](https://github.com/noetl/server/commit/0e230915969d75c2e42ed76f7b26e71bd1915223)), closes [noetl/server#66](https://github.com/noetl/server/issues/66) [noetl/ai-meta#60](https://github.com/noetl/ai-meta/issues/60)

## [2.19.3](https://github.com/noetl/server/compare/v2.19.2...v2.19.3) (2026-06-05)

### Bug Fixes

* **orchestrator:** emit playbook.failed on command.failed instead of stalling ([cabf470](https://github.com/noetl/server/commit/cabf47019253a0f53c98abc468083c1c1ce4d94a)), closes [noetl/server#62](https://github.com/noetl/server/issues/62) [noetl/ai-meta#58](https://github.com/noetl/ai-meta/issues/58)
* **parser:** resolve tool.kind:workbook references to inline actions ([e7d0de3](https://github.com/noetl/server/commit/e7d0de39ff0a87466795cba9bddb3e2db3ef148f)), closes [ai-meta#56](https://github.com/noetl/ai-meta/issues/56) [noetl/cli#54](https://github.com/noetl/cli/issues/54) [noetl/server#64](https://github.com/noetl/server/issues/64) [noetl/ai-meta#59](https://github.com/noetl/ai-meta/issues/59)
* **playbook:** accept flat (name-as-field) pipeline shape ([09e0e47](https://github.com/noetl/server/commit/09e0e47d67f91ab86515f1382ea0e534fb48ae3d)), closes [noetl/cli#53](https://github.com/noetl/cli/issues/53) [noetl/server#60](https://github.com/noetl/server/issues/60) [noetl/ai-meta#57](https://github.com/noetl/ai-meta/issues/57)

## [2.19.2](https://github.com/noetl/server/compare/v2.19.1...v2.19.2) (2026-06-05)

### Bug Fixes

* **execute:** playbook workload reaches all steps via input alias + merge ([d218326](https://github.com/noetl/server/commit/d218326dc482bdc9ecb611b41e33a50adcb9eb9e)), closes [noetl/ai-meta#56](https://github.com/noetl/ai-meta/issues/56)

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
