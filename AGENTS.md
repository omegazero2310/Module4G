# Repository Guidelines

## Project Structure & Module Organization

This repository is a Rust workspace with a Tauri/React desktop application:

- `crates/modemd/`: Windows service, serial-port ownership, modem workflows, validation, and SQLite storage.
- `crates/modem-proto/`: protobuf contract and generated tonic types. Treat `proto/modemd/v1/modem.proto` as the API source of truth.
- `crates/modem-sim/`: deterministic named-pipe simulator for development without hardware.
- `modem-app/src/`: React UI and styling.
- `modem-app/src-tauri/`: native Tauri bridge; browser code must not access the named pipe directly.
- `scripts/`: elevated Windows service installation and removal scripts.

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

## Coding Style & Naming Conventions

Use Rust 2024 idioms and `rustfmt`. Name modules and functions `snake_case`, types `PascalCase`, and constants `SCREAMING_SNAKE_CASE`. Avoid `unsafe` in library code. TypeScript uses two-space indentation, `PascalCase` React components, and `camelCase` values. Keep modem commands inside dedicated daemon workflows; do not weaken guarded-console validation.

## Testing Guidelines

Add focused unit tests for framing, validation, encoding, persistence, and state transitions. Test names should describe behavior, such as `detects_bare_prompt_and_chunked_lines`. Simulator behavior must remain deterministic. Before submitting, run the workspace tests, Rust checks, and frontend production build. Hardware-dependent changes should document the A7670 model, COM port, firmware, and acceptance steps used.

## Commit & Pull Request Guidelines

History currently contains only an `init` commit, so no established convention exists. Use short imperative subjects, optionally scoped, for example `sms: handle CMGS prompt timeout`. Pull requests should explain behavior and risks, list verification commands, link relevant issues, and include screenshots for UI changes. Call out protocol, migration, installer, or hardware compatibility changes explicitly.

## Security & Configuration

Never log phone numbers, message bodies, audio data, or unrestricted AT output. Preserve the local-only named-pipe ACL and remote-client rejection. Production data belongs under `%ProgramData%\A7670 Modem\`; uninstall must retain it unless purge is explicitly requested.
