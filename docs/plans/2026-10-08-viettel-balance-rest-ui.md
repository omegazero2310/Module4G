# Viettel SIM balance checks through REST and UI

Status: software implementation in progress; source inspection complete and physical acceptance pending. See balance-execution-ledger.md and ../../docs.md for implementation decisions and client instructions.

## Goal and scope

An authenticated REST client or desktop user requests a balance check of the Viettel SIM installed in the A7670C-LANS modem. The daemon sends the verified carrier SMS query, persists and parses the reply, and exposes the result through authenticated REST and the existing Tauri/local-pipe boundary.

This does not enable incoming SMS commands from arbitrary senders. Scope is balance-related service and UI improvements, not a general UI redesign.

## Evidence and prerequisite

This proposal is based on the supplied repository instructions. Shell and Node filesystem access failed with a Windows sandbox helper setup error, so source files and planning skill files could not be read. Exact symbols, filenames, existing balance behavior, migration numbers, and current UI labels must be verified before implementation.

The repository instructions identify an existing Viettel balance workflow in `crates/modemd/src/windows_host/`, balance persistence, SMS synchronization, an authenticated REST listener, and a Tauri client. Reuse and extend these components after inspection.

First confirm the installed SIM's prepaid/postpaid account type, the carrier-approved SMS destination/body, expected reply senders, fees, and a sanitized response fixture. Do not assume a postpaid charge query returns prepaid cash balance, or mistake data/package allowances for money. If the requested monetary balance has no supported SMS query, resolve the transport requirement before implementing; do not silently substitute USSD.

## Proposed behavior

1. REST/UI reserves a durable balance-check operation before dispatch.
2. The daemon schedules the verified carrier query through the existing command actor and SMS submission workflow.
3. Successful SMS submission transitions to waiting for the carrier reply; it does not complete the balance check.
4. Normal SMS synchronization persists the incoming reply before deleting its exact SIM slot. Balance processing runs from that durable record.
5. A conservative parser extracts the main monetary balance, currency, and observation time. The operation completes only for a valid correlated reply.
6. REST clients poll the operation; the UI refreshes via Tauri. The last successful balance remains available while a new check is running or fails.

## REST contract proposal

Use dedicated balance resources, preserving the existing communications and minimal health contracts. All new routes require the existing enabled REST gate and bearer authentication; never expose stored tokens.

| Route | Behavior |
| --- | --- |
| `POST /api/v1/balance-checks` | Accept `{ "request_id": "opaque-client-key" }`; durably reserve and return `202 Accepted` with the operation. |
| `GET /api/v1/balance-checks/{id}` | Return operation state and its own result, if available; unknown IDs return `404`. |
| `GET /api/v1/balance` | Return the latest successful observation, its timestamp, freshness, and active check ID. Before any success, return an explicit unavailable result rather than zero. |

Use the existing REST envelope/casing conventions after inspection. Proposed operation data fields: `id`, `request_id`, `status`, `created_at`, `updated_at`, `completed_at`, `balance`, and sanitized `failure_reason`. A successful balance contains integer `amount_vnd` and `observed_at`; avoid floating point monetary parsing. Missing result is null, never a stale value masquerading as this operation's result.

States: `queued`, `sending`, `waiting_reply`, `succeeded`, `failed`, `timed_out`, `send_unknown`. Define allowed transitions centrally. `send_unknown` preserves indeterminate submission and may resolve through a safely correlated reply within its original deadline; after expiry it becomes `timed_out` with submission uncertainty retained.

Idempotency must be atomic with reservation. Repeating a request ID returns the same operation without another SMS. Define the key namespace explicitly relative to communications. Reject a different request while a check is active with `409` and a reference to the active check. Reject requests within a configured cooldown with `429` and `Retry-After`. Reject new dispatch while hardware cannot accept it with `503`, while permitting retrieval of persisted results. Resolve duplicate IDs before availability/cooldown checks so retries can retrieve their original operation.

Polling is the initial REST reporting mechanism. A push callback is separate scope: existing communication webhooks have a deliberately limited payload and must not be extended to carry balances.

## Service reliability and parsing

- Reuse the existing actor, prompt/final-result deadlines, SMS states, and framing recovery. Do not create another serial owner or hold the actor while awaiting a carrier SMS.
- Permit only one active balance query for this modem. Correlate using a verified carrier sender, expected response structure, persisted receive identity, operation window, and known dispatch time. Sender identity alone is insufficient.
- Reject unrelated, old, duplicate, ambiguous, malformed, and incomplete multipart replies. Preserve the normal inbox record and mark parser errors without guessing a balance.
- Support observed Viettel number separators, Vietnamese/ASCII wording, and UCS2 through the existing decoder. Explicitly distinguish main balance from bonus, data allowance, and postpaid debt. Fail closed for unknown formats; accept zero as a valid parsed amount.
- Carrier replies do not carry the REST request ID. Late replies can be ambiguous, particularly after a previous timeout; define a cooldown/quarantine policy and leave ambiguous results unassociated rather than claiming exact correlation.
- Persist an absolute reply deadline. Resume pending reply processing after service restart. Do not resend a query whose dispatch might already have happened; recover unresolved send attempts as indeterminate. Process already persisted candidate replies before expiring a resumed operation.
- Make successful observation, operation completion, and consumed reply linkage transactional and idempotent. A reply may complete at most one check.
- Preserve existing outbound SMS evidence classification for health. Carrier silence, parser failure, and stale balance are not new modem/dispatch failure classifications; genuine transport failures keep their existing classification.

## Storage and transport

Inspect existing balance tables before adding a forward SQLite migration. Reuse compatible records and add durable operation state, request-key uniqueness, dispatch/reply timestamps, deadline, source SMS linkage, integer amount, and sanitized error code as needed. Preserve SMS, call, audio, settings, communications, and webhook-outbox records.

Retain a SIM association where the current implementation supports one, and invalidate or mark previous observations unavailable after detected SIM replacement. Never present another SIM's balance as current. Do not introduce sensitive SIM identifiers into diagnostics.

Add compatible named-pipe request/response types and Tauri commands for starting a check, reading its status, and reading the latest balance. Browser code calls only Tauri. Update simulator envelopes, consumer types, and serialization tests together. Inspect the protobuf contract and update it only if necessary; reserve removed identifiers.

## UI changes

- Add or improve the existing balance view with main balance in VND, last successful observation time, and a visible freshness label. A carrier reply is a snapshot, not a continuously live balance.
- Provide an accessible `Check balance` button. Show queued, sending, waiting, success, timeout, and sanitized failure states; disable duplicate submission and explain cooldown.
- Preserve the last successful observation when refresh fails, with its original timestamp and a stale indication. Show unavailable before first success, separately from a valid zero balance.
- Resume active-check display after app reload. Use bounded polling with cleanup on unmount and backoff for transport failures; stop when the operation is terminal.
- Put allowed carrier profile/freshness settings in Settings only if needed after inspecting existing configuration. Do not turn this feature into an arbitrary SMS/AT execution endpoint.
- Diagnostics may show operation IDs, phase, timing, and generic outcome only. Keep carrier messages and balance amounts out of logs/diagnostics; expose the numeric result only through intended authenticated result/UI surfaces.

## Implementation sequence

1. Inspect AGENTS files, existing balance/service/storage/REST/Tauri/UI/simulator code, and acceptance documents. Confirm the carrier SMS query on the installed account type using sanitized fixtures. Finalize route casing, states, timing/cooldown policy, and data reuse.
2. Add parser/state-machine tests, implement conservative reply correlation and restart/deadline recovery, then add the smallest forward storage migration with retention tests.
3. Add authenticated REST resources with transactional idempotent reservation and dispatch recovery; keep communication webhook and health behavior intact.
4. Extend local protocol, Tauri commands, types, and deterministic simulator scenarios. Add success, zero balance, timeout, unknown send, malformed/unrelated/duplicate reply, restart, and concurrent request scenarios.
5. Implement the balance UI and focused accessibility/state tests. Capture screenshots for review.
6. Run regression checks and hardware acceptance. Update API documentation and the existing hardware/automation acceptance documents with verified behavior and evidence.

## Verification and acceptance

Automated coverage: parser formats/zero/malformed/ambiguous/multipart responses; exact-once reply consumption; idempotency under concurrent requests; auth and REST enablement; cooldown/busy behavior; deadline expiry and restart recovery without duplicate SMS; stale/unavailable/zero UI states; migration retention; diagnostic/token redaction; unchanged health and communication webhook contracts.

Run focused tests first, then `cargo fmt --all --check`, `cargo check --workspace`, `cargo test --workspace`, `npm.cmd test`, and `npm.cmd run build` from the documented directories. No installer or driver changes are expected.

Hardware evidence: A7670 model, COM port, ATI/CGMR, relevant SMS configuration readbacks, verified query destination/body, reply identity/format and elapsed time in a restricted acceptance record. Keep private payloads out of commits. Exercise normal/zero balance where feasible, carrier silence, duplicate requests, unrelated/late replies, service restart after submission, and SMS receipt during a call. Record current balance comparison against a trusted carrier source close to the observation time.

Done when REST and UI initiate the same durable workflow, a verified carrier reply yields an authenticated timestamped balance, retries/restarts do not duplicate dispatch, failures preserve the last successful snapshot with correct freshness, and required regression/hardware evidence is documented.
