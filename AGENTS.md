# Repository Guidelines

## Project Structure and Runtime Boundaries

This is a Windows-focused Rust 2024 workspace with a Tauri 2 / React 19 desktop app for one SIMCom A7670C-LANS modem.

- `crates/modemd/`: daemon core and Windows service binary. The library contains AT framing, serial discovery/command scheduling, SMS/PDU handling, SQLite storage, AMR-NB validation, call workflows, and REST/webhook integration. `src/windows_host/` owns the service host, local named-pipe server, JSON/legacy request routing, SMS synchronization, delivery setup, Viettel balance workflow, REST listener, and webhook worker. `src/integration.rs` owns the public REST contract, lifecycle mapping, webhook delivery, and sanitized diagnostic events; `src/storage/integration.rs` owns durable REST communication and webhook-outbox persistence.
- `crates/modem-proto/`: versioned protobuf schema and generated tonic types. `proto/modemd/v1/modem.proto` is the API contract source; reserve fields/names rather than reusing them. The current Windows pipe transport is newline-delimited JSON with a retained legacy line protocol, not a tonic server.
- `crates/modem-sim/`: deterministic Windows named-pipe simulator. Keep its JSON envelopes, legacy replies, IDs, and failure scenarios compatible with the host contract.
- `modem-app/src-tauri/`: native Tauri commands and named-pipe client. This is the only UI-layer component permitted to access `\\.\pipe\a7670-modemd-v1`.
- `modem-app/src/`: React UI, pages, formatting helpers, types, and Vitest tests. Browser code calls Tauri commands only. The Settings page manages integration settings without reading stored secrets; the Diagnostics page can display sanitized integration activity.
- `scripts/`: elevated service installation/removal and direct AMR-upload diagnostic scripts.
- `a7670c-sms-send-delivery-status.md` and `a7670c-automation-plan (2).md`: hardware behavior and acceptance context; update them when implementation behavior or acceptance evidence changes.

The production service owns the serial port. Do not add direct COM or named-pipe access to React/browser code, and do not create a second production serial-port owner. The app can target `modem-sim` during development or the installed service in production; do not run both on the same pipe.

Production data is `%ProgramData%\A7670 Modem\modemd.sqlite3`. The service pipe is local-only, rejects remote clients, and has an explicit Windows DACL. Preserve those properties.

## REST and Webhook Integration

The daemon exposes `POST /api/v1/communications` on the configurable REST bind address (default `0.0.0.0:5069`). The listener rejects requests unless REST is enabled and a stored bearer token matches; REST cannot be enabled without a token. The endpoint accepts immediate `sms` and `call` communications with a non-empty opaque `request_id`, which is the idempotency key. The daemon generates the communication UUID (`data.id`), returns the normalized destination as `data.contact`, and replays the original communication for a duplicate `request_id`.

For calls, `content` is the case-insensitive name of a validated uploaded AMR-NB file. Empty content selects the current audio file and must fail validation when no current audio exists. Future `send_at` scheduling is not supported. Preserve the request/response casing and the limited webhook payload: event type plus `id`, `request_id`, and nullable `failure_reason` only.

Only REST-created communications create lifecycle webhooks. Persist them in SQLite before delivery, preserve event order per communication, disable HTTP redirects, and retry a failed webhook until it receives a 2xx response. SMS/call state reconciliation produces `communication.sent`, `communication.delivered`, or `communication.failed` as appropriate. A remote or local hang-up after an answered call is delivered; an unanswered local hang-up remains sent and must not create a failure webhook. Busy, no-answer, signaling timeout, and terminal call errors are failed, with a sanitized non-empty failure reason.

REST and webhook tokens are secrets: persist them only through the integration settings flow, never return their values through named-pipe/Tauri responses, and never place them in diagnostics or logs. The REST listener bind address applies on service restart; REST enablement, webhook URL, and token changes apply to request/delivery handling immediately. This is a trusted-LAN HTTP integration: plain HTTP exposes tokens and communication data in transit, so retain firewall/network isolation or use a TLS-terminating reverse proxy.

`MODEMD_INTEGRATION_DEBUG=1` enables short-lived diagnostics. Retain no more than 200 sanitized, in-memory events and clear them on daemon restart. Events may include timestamps, phase/outcome, status code, IDs, channel, byte count, payload SHA-256, elapsed time, and generic summaries; never include tokens, headers, phone numbers, body/content, raw JSON/PDU, webhook response text, or URL query values.

## Build, Test, and Development Commands

Run commands from the repository root unless stated otherwise:

```powershell
cargo fmt --all --check
cargo check --workspace
cargo test --workspace
cargo run -p modem-sim

cd modem-app
npm.cmd install
npm.cmd test
npm.cmd run build
npm.cmd run tauri dev
```

Start either the simulator or service before launching the Tauri app. `modemd --scan` is a read-only physical-port discovery/initialization aid; `modemd --console` runs the named-pipe host outside SCM for local debugging. Build the release service with `cargo build -p modemd --release`, then run `scripts\install-service.ps1` from an elevated PowerShell. Uninstall retains ProgramData unless explicitly passed `-PurgeData`.

For physical AMR investigation, stop the service first, then run `scripts\diagnose-a7670-upload.ps1 -Port COMx`. The script intentionally leaves diagnostic files on the modem; document and clean them up during acceptance.

## Behavior That Must Be Preserved

- Keep `modemd` as the sole production serial-port owner. Route all UI operations through Tauri and the local service protocol.
- The hardware monitor and command actor serialize AT access. Health probes/reconnects must not interrupt a command, interactive transfer, SMS submission, call, or URC processing.
- Treat SIM SMS storage as a durable inbox queue: parse and commit messages/status reports to SQLite before exact-slot `AT+CMGD=<index>` deletes. Delete each persisted slot once in descending index order; never issue a broad delete.
- Preserve SMS states: `sending`, `submitted`, `send-failed`, `send-unknown`, `delivery-pending`, `delivered`, `delivery-failed`, and `delivery-unknown`. `+CMGS` followed by final `OK` completes submission; handset delivery never blocks `SendSms`.
- Delivery configuration uses and verifies `CPMS`, `CSMP`, and `CNMI`. Capability failures must remain visible but cannot turn a successful submission into a send failure. Expire pending reports after 24 hours, while allowing a late terminal report to replace `delivery-unknown`.
- Correlate delivery reports conservatively using message reference, normalized peer, and timestamps. TP-MR reuse or ambiguity must remain uncorrelated.
- Preserve multipart identity, whitespace, encoding/DCS metadata, and incomplete-part visibility. Malformed PDU/UCS2 must remain lossless and must not panic.
- Interactive uploads and SMS submissions have distinct prompt/final-result deadlines and must resynchronize framing after timeout. An indeterminate submission is `send-unknown`, not `send-failed`.
- Accept validated AMR-NB only. Preserve safe module paths, paced uploads, duration metadata, audio-library selection, and call-history linkage.
- Keep REST request idempotency atomic with communication reservation and dispatch. Do not create lifecycle webhooks for UI-originated SMS/calls, reorder events for the same REST communication, follow webhook redirects, or expose persisted integration tokens.
- Call lifecycle mapping is externally visible: an answered call that ends is `delivered`; busy/no-answer/signaling-timeout/terminal errors are `failed`; a pre-answer local hang-up remains `sent` and emits no failure webhook.
- Keep the guarded console validation and request/response log redaction. Never log numbers, message bodies, audio payloads, raw PDU, balance content, or unrestricted AT output.

## Code Style and Change Scope

Use Rust 2024 idioms and `rustfmt`; use `snake_case` functions/modules, `PascalCase` types, and `SCREAMING_SNAKE_CASE` constants. `crates/modemd/src/lib.rs` forbids `unsafe`; do not weaken that restriction. Windows host code may use narrowly scoped Win32 FFI only where required for service/pipe security, and must release acquired resources.

TypeScript uses two-space indentation, `PascalCase` components, and `camelCase` values. Preserve existing Tauri command names and JSON field casing unless all consumers (host, client, UI, simulator, tests, and protocol documentation) change together. Keep UI changes accessible: retain keyboard activation for interactive rows and do not surface secret modem data in diagnostics.

Keep protocol and schema transitions explicit. For `.proto` changes, update generated-code inputs and all relevant transports; never reuse protobuf fields, enum values, or reserved names. For SQLite changes, add a forward migration and tests that prove existing SMS, calls, balance records, audio, settings, REST communications, and pending webhook outbox records are retained or intentionally transformed.

## Testing and Hardware Acceptance

Add focused tests beside the Rust module under test and use Vitest/Testing Library for UI behavior. Cover framing/URC separation, validation, PDU/GSM7/UCS2 decoding, multipart assembly, storage migrations and archive ordering, delivery correlation/expiry, timeout recovery, call state transitions, simulator determinism, REST authorization/validation/idempotency, durable ordered webhook retry, call-to-webhook lifecycle mapping, integration-secret redaction, diagnostics sanitization, and Tauri serialization/log redaction as applicable.

Before handing off a code change, run the narrow relevant tests and normally also:

```powershell
cargo fmt --all --check
cargo test --workspace
cd modem-app; npm.cmd test; npm.cmd run build
```

Hardware-dependent changes require documented evidence: A7670 model, COM port, `ATI`, `AT+CGMR`, relevant command readbacks/raw prompt behavior, and reproducible acceptance steps. In particular, delivery-report work must record `AT+CNMI=?`, `AT+CNMI?`, `AT+CSMP?`, and `AT+CPMS?`, then test reachable/unreachable recipients, a service restart before receipt, receipt during a call, and TP-MR reuse.

## Commit, Review, and Security Expectations

Use short imperative commit subjects, optionally scoped (for example, `sms: handle CMGS prompt timeout`). Keep changes focused; call out protocol/schema changes, migrations, service installer changes, SMS-state changes, REST/webhook contract or security changes, and hardware compatibility in review notes. Include UI screenshots when visuals change.

Never commit generated build output from `target/` or `modem-app/dist/`, runtime databases, modem diagnostic payloads, integration bearer tokens, webhook captures, or private logs. Treat `output_test.amr` as a local diagnostic artifact, not an application asset. Do not weaken local pipe ACLs, remote-client rejection, REST authentication, webhook payload minimization, console guards, or data-retention behavior without an explicit security review.
