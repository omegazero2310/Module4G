# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust 2024 workspace with a Tauri 2/React 19 desktop application:

- `crates/modemd/`: Windows service and sole serial-port owner. `main.rs` coordinates the named-pipe API, SMS synchronization/archival, delivery reports, calls, balance checks, and service lifecycle; `hardware.rs` owns COM discovery, AT I/O, reconnects, health probes, SMS submission, and raw uploads.
- `crates/modem-proto/`: protobuf contract and generated tonic types. Treat `proto/modemd/v1/modem.proto` as the API source of truth.
- `crates/modem-sim/`: deterministic named-pipe simulator for development without hardware.
- `modem-app/src/`: React UI and styling for modem status, SMS, audio, calls, balance, settings, and the guarded console.
- `modem-app/src-tauri/`: native Tauri bridge. Browser code must not access the named pipe or serial port directly.
- `scripts/`: elevated Windows service install/uninstall scripts and the direct-hardware AMR upload diagnostic.
- `a7670c-sms-send-delivery-status.md`: SMS submission and delivery-report behavior, edge cases, and hardware acceptance plan.

Rust unit tests live beside their modules in `#[cfg(test)]` blocks. Generated build output belongs in `target/` or `modem-app/dist/` and must not be committed.

## Build, Test, and Development Commands

Run from the repository root unless noted:

```powershell
cargo fmt --all --check       # Check Rust formatting
cargo check --workspace       # Type-check every Rust crate
cargo test --workspace        # Run all Rust tests
cargo run -p modem-sim        # Start the hardware-free simulator
cd modem-app
npm.cmd install               # Install locked frontend dependencies
npm.cmd run build             # Type-check and create the production UI bundle
npm.cmd run tauri dev         # Run the desktop app during development
```

Start the simulator or service before the Tauri application.

For a physical AMR upload investigation, first stop the service so it does not own the port, then run `scripts\diagnose-a7670-upload.ps1 -Port COMx`. The script intentionally retains diagnostic files on the modem; document and clean them up during hardware acceptance.

## Feature Invariants

- `modemd` is the only production serial-port owner. Keep AT commands in daemon workflows and route UI access through the Tauri backend.
- Treat modem SMS storage as a durable inbox queue: parse and commit every message/status report to SQLite before issuing exact-slot `AT+CMGD=<index>` deletes. Delete each persisted slot once, in descending index order. Never replace this with a broad delete.
- Preserve SMS submission states: `sending`, `submitted`, `send-failed`, `send-unknown`, `delivery-pending`, `delivered`, `delivery-failed`, and `delivery-unknown`. `+CMGS` plus final `OK` completes submission; handset delivery must not block `SendSms`.
- Delivery tracking configures and verifies `CPMS`, `CSMP`, and `CNMI`. Capability failures must be visible through status/record fields and must not turn a successfully submitted SMS into a send failure. Pending delivery expires after the configured 24-hour validity window, but a late terminal report may still update it.
- Correlate status reports conservatively by message reference, normalized peer, and timestamps. TP-MR reuse or ambiguous matches must remain uncorrelated rather than updating the wrong message.
- Preserve multipart identity, original whitespace, encoding metadata, and incomplete-part visibility during synchronization. Malformed PDU input must remain lossless and must not panic.
- Keep the 10-second idle health probe and reconnect behavior non-invasive: do not interrupt active commands, raw uploads, SMS submissions, calls, or URC processing.
- Interactive uploads and SMS submissions have separate prompt/final-result deadlines and must resynchronize the parser after timeout. Do not collapse an indeterminate submission into a definite failure.
- Audio uploads accept validated AMR-NB data only; retain safe module paths, upload pacing, duration metadata, library selection, and call-history linkage.

## Coding Style & Naming Conventions

Use Rust 2024 idioms and `rustfmt`. Name modules and functions `snake_case`, types `PascalCase`, and constants `SCREAMING_SNAKE_CASE`. The daemon library forbids `unsafe`; keep it that way. TypeScript uses two-space indentation, `PascalCase` React components, and `camelCase` values. Keep protocol state transitions explicit and do not weaken guarded-console validation.

## Testing Guidelines

Add focused unit tests for framing, URC separation, validation, PDU/GSM7/UCS2 decoding, multipart assembly, storage migrations, archive ordering, delivery correlation/expiry, upload timeouts, health recovery, and call state transitions. Test names should describe behavior, such as `detects_bare_prompt_and_chunked_lines`. Simulator behavior must remain deterministic and should cover new named-pipe commands or modem responses. Before submitting, run formatting, workspace checks/tests, and the frontend production build. Hardware-dependent changes must document the A7670 model, COM port, `ATI`, `AT+CGMR`, relevant AT readbacks, and acceptance steps used.

## Commit & Pull Request Guidelines

Use short imperative subjects, optionally scoped, for example `sms: handle CMGS prompt timeout`. Keep commits focused across protocol, daemon, simulator, Tauri bridge, and UI changes when practical. Pull requests should explain behavior and risks, list verification commands, link relevant issues, and include screenshots for UI changes. Call out protobuf changes, SQLite migrations, SMS state-machine changes, installer changes, and hardware compatibility explicitly.

## Security & Configuration

Never log phone numbers, message bodies, audio payloads, raw PDU content, or unrestricted AT output. Log only sanitized response/control-byte metadata needed for diagnosis. Preserve the local-only named-pipe ACL and remote-client rejection. Production data belongs under `%ProgramData%\A7670 Modem\`; uninstall must retain it unless purge is explicitly requested with `-PurgeData`.
