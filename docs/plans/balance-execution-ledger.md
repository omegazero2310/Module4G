# Execution ledger — plan: 2026-10-08-viettel-balance-rest-ui.md

User authorized execution and chose the current folder. Branch: codex/viettel-balance-api.

Ruling: retain the existing TK-to-191 carrier profile, with hardware acceptance pending — this is already the repository's UI/service behavior; no carrier transport is silently substituted.
Ruling: reuse the existing SMS dispatcher and periodic durable inbox synchronization — this preserves actor serialization, health evidence, and exact-slot archive semantics.
Ruling: use separate balance request-key namespace, 120-second reply deadline, 60-second successful/failed-query cooldown, and 5-minute timeout quarantine — SMS has no request key, so uncertain late replies must not be attributed to a subsequent request.
Ruling: UI uses additive start/get/latest commands; retain existing legacy check/history commands through the same operation workflow.
Ruling: no SIM identity is currently available in HardwareState — snapshot association cannot be proven after an undetected SIM swap; document this hardware limitation rather than invent an identity.

Pre-flight: storage operation state is shared by service worker, REST, pipe and UI; REST snake_case timestamps use RFC3339 and pipe reuses the same additive data contract. Existing balance history remains camelCase.

Tasks:
1. Complete: source inspection; clean baseline Rust workspace and UI tests.
2. Complete: main monetary parser RED/GREEN; migration 11 and 8 workflow tests. Additional RED/GREEN checks reject expired dispatch, mark failed refresh stale, reject conflicting/unparseable main labels, and preserve integer precision.
3. Complete: authenticated routes, shared worker and inbox preparation; end-to-end REST RED/GREEN test proves replay, conflict, polling result and no webhooks.
4. Complete: additive pipe/Tauri commands and deterministic simulator RED/GREEN scenarios (success, zero, timeout, unknown, malformed, unrelated, ambiguous).
5. Complete: UI with resume, duplicate suppression, timestamped balance, stale/unavailable/zero, bounded polling/backoff, cooldown, history keyboard activation; 6 focused tests and synthetic Tauri browser visual flow.
6. Regression: final Rust workspace suite passed all 147 tests; cargo fmt --all --check, cargo check --workspace and git diff --check passed. All 18 UI tests passed in separate processes, and the production tsc/Vite build passed. Hardware acceptance remains pending.

Final review: accepted and fixed both findings. A shared asynchronous store guard covers the entire inbox snapshot, durable persistence, and exact-slot archival; the concurrency test first demonstrated duplicate deletion and now proves one deletion. Delivery configuration happens before balance dispatch is claimed, and the absolute deadline follows the SMS request to the serial actor. The actor expiry test first demonstrated an expired write and now proves zero writes; the host test verifies deadline propagation, known unsent failure, and no enqueue for already expired queries. No second review pass is needed under the executing-plans workflow.

Environment rulings: default shell sandbox fails helper setup, so approved elevated execution was used. Existing debug PDB hit LINK1140 and D: ran out of space. Generated incremental cache cleanup freed some space; older files denied deletion and were left alone. Final Rust checks use C: task-output rust-target, incremental disabled and debug symbols disabled. Default Vitest workers timed out before collecting tests; bundled Node with vmThreads ran all 18 tests. Existing dist/assets denied replacement; the same tsc/Vite build succeeded with an output directory in the C: task workspace. No dependency or production build-profile changes were made.

Final UI rerun ruling: shared vmThreads worker reuse exposed cross-file API mock and DOM contamination (the balance tests saw the SMS page). Each of the five files passed in its own Vitest process with bundled Node and vmThreads: 7 + 6 + 2 + 1 + 2 = 18 tests. No product code changes were made for this test-runner isolation issue.

Visual evidence: balance-waiting.png and balance-success.png in the task output directory, using synthetic Tauri responses only. Images were inspected; physical modem outcomes are not claimed.

Ruling: snapshot freshness is five minutes, and a later failed/timed-out refresh marks the previous snapshot stale — preserves a visible amount without claiming a new successful observation.
Ruling: terminal replay clears the UI request key, but a transport error retains it — subsequent deliberate checks get new keys, while lost-response retries remain idempotent.
