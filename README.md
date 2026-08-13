# A7670 Modem

Windows 10/11 x64 desktop software for one SIMCom A7670C-LANS modem. The solution pairs a Rust Windows service (the only production owner of the modem serial port) with a Tauri 2 / React desktop application. It supports SMS, delivery reports, AMR-NB audio upload and calls, balance checks, local history, and an authenticated REST/webhook integration.

## Prerequisites

- Windows 10 or 11 x64.
- Rust stable with the MSVC toolchain (`rustup default stable-x86_64-pc-windows-msvc`).
- Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK.
- Node.js 20 or newer and npm.
- Microsoft Edge WebView2 Runtime (normally already present on supported Windows installations).
- An A7670C-LANS modem for physical-modem use. Builders must separately obtain
  and locally stage the SIMCom Windows 10 x64 filter and serial drivers before
  creating the production installer.

`protoc` is supplied by the Rust build, so it does not need a separate installation.

## Project structure

| Path | Purpose |
| --- | --- |
| `crates/modemd/` | Daemon core and `modemd.exe` Windows service. Owns AT/serial communication, workflows, SQLite, named-pipe host, REST, and webhooks. |
| `crates/modem-proto/` | Versioned protobuf API source in `proto/modemd/v1/modem.proto` and generated Rust types. It is the public contract reference; the current Windows transport itself is JSON over a named pipe. |
| `crates/modem-sim/` | Deterministic Windows named-pipe simulator for UI and protocol development without hardware. |
| `modem-app/src/` | React 19 UI: dashboard, SMS, calls/audio, balance, settings, diagnostics, types, formatters, and Vitest tests. |
| `modem-app/src-tauri/` | Tauri native layer. It is the sole UI component that can access the local service pipe and exposes Tauri commands to React. |
| `scripts/` | Elevated service install/remove scripts and the direct AMR upload diagnostic utility. |
| `a7670c-*.md` | SIMCom hardware behavior, acceptance context, and delivery-status notes. |

## How it works

### Runtime boundary

```text
React UI
  -> Tauri commands (Rust)
    -> \\.\pipe\a7670-modemd-v1 (local-only JSON; legacy lines retained)
      -> modemd Windows service
        -> serialized AT command actor -> A7670 serial/AT port
        -> SQLite: %ProgramData%\A7670 Modem\modemd.sqlite3

External LAN client -> authenticated REST listener -> modemd -> durable webhook outbox -> webhook receiver
```

The browser/React process never opens a COM port or named pipe. `modemd` serializes AT access so health checks, SMS, uploads, calls, and unsolicited modem results do not interrupt one another. The service pipe rejects remote clients and is protected with a Windows ACL.

### Main functional areas

- **AT, hardware, and discovery** (`at.rs`, `hardware/`): validates guarded console commands, identifies the correct modem port, initializes it, frames command responses/prompts, separates unsolicited results, and performs interactive transfers with timeout recovery.
- **SMS** (`sms.rs`, `windows_host/host/sms_workflow.rs`, `storage/sms.rs`): validates and encodes outbound SMS; tracks submission separately from handset delivery; parses GSM7/UCS2/PDU records and multipart messages. During synchronization it saves each inbound message or delivery report before deleting that exact modem storage slot.
- **Audio and calls** (`audio.rs`, `call.rs`, `call_workflow/`, `storage/audio.rs`, `storage/calls.rs`): accepts only validated AMR-NB audio, uploads it in paced chunks, synchronizes the modem library and current selection across PCs through redundant module-side manifests, then manages dialing, answered/ended calls, failures, and call history.
- **Balance** (`windows_host/host/balance.rs`, `storage/balance.rs`): runs the configured USSD/balance workflow, parses the configured result where possible, and archives the check.
- **Settings and storage** (`settings.rs`, `storage/`): validates modem and integration settings; stores settings, records, audio metadata, and integration outbox data in SQLite. Migrations preserve existing data.
- **REST and webhooks** (`integration.rs`, `storage/integration.rs`): serves `POST /api/v1/communications` when enabled and supplied with a matching bearer token. `request_id` makes requests idempotent. REST-created SMS/calls create ordered lifecycle webhooks stored before delivery and retried until a 2xx response; redirects are not followed. Tokens are never returned through the pipe/UI or diagnostics.
- **Windows host and simulator** (`windows_host.rs`, `modem-sim/`): the host runs as an SCM service or `--console` process, exposes the pipe API, starts the REST listener, and owns the database. The simulator implements compatible deterministic JSON/legacy replies for development; never run it on the same pipe as the service.

## Build and test

Run these commands from the repository root unless a command changes directory.

```powershell
# Format verification and Rust compile/test suite
cargo fmt --all --check
cargo check --workspace
cargo test --workspace

# Install UI dependencies once (or after package-lock changes)
cd modem-app
npm.cmd install

# UI unit tests and production web build
npm.cmd test
npm.cmd run build
```

For formatting fixes during development, use `cargo fmt --all`. `npm.cmd` is used deliberately on Windows to avoid PowerShell command-resolution differences.

## Development and local diagnostics

### Run against the simulator

In terminal 1:

```powershell
cargo run -p modem-sim
```

In terminal 2:

```powershell
cd modem-app
npm.cmd run tauri dev
```

The simulator listens on `\\.\pipe\a7670-modemd-v1`; start it before the app. It provides predictable status and request behavior but does not use physical hardware.

### Run against a physical modem without installing the service

First build the daemon, then use one of these read/development modes:

```powershell
cargo build -p modemd

# List serial ports and attempt modem initialization; does not start the service.
.\target\debug\modemd.exe --scan

# Run the production named-pipe host interactively. Stop with Ctrl+C.
.\target\debug\modemd.exe --console
```

Start the Tauri app only after `--console` is running. Do not start `modem-sim` or the installed service at the same time: all use the same pipe name.

For low-level AMR troubleshooting, stop the service before running `scripts\diagnose-a7670-upload.ps1 -Port COMx`. That script intentionally leaves diagnostic files on the modem.

## Release builds and deployment

The repository does not distribute SIMCom driver files. Before building, obtain
the authorized `Windows10.zip` package and stage the minimal signed payload
locally in `third_party\simcom\windows10-x64-serial`. Follow the download-source,
archive-verification, extraction, exact-file-copy, and bundle instructions in
[the SIMCom driver preparation guide](third_party/simcom/windows10-x64-serial/README.md).
The staged INF/CAT/SYS files are gitignored and must not be committed. Do not
copy the complete vendor archive into the repository or installer.

From the repository root, prepare the local payload with:

```powershell
.\scripts\prepare-simcom-driver.ps1 -SourceZip 'D:\path\to\Windows10.zip'
```

Build the combined desktop and service installer:

```powershell
cd modem-app
npm.cmd install
npm.cmd run tauri build
```

This is the single production release entry point. It first validates the bundled SIMCom driver files and catalog signatures, builds `modemd.exe` in release mode, builds the frontend and desktop executable, and then creates a per-machine NSIS installer containing both executables and only the required x64 filter/serial driver packages. A driver validation, daemon build, or staging failure fails the Tauri build. Locate the generated installer without relying on a Cargo target-layout detail:

```powershell
Get-ChildItem -Recurse -Filter "*setup*.exe" .\src-tauri\target, ..\target -ErrorAction SilentlyContinue
```

Before shipping, normally run the full verification sequence in **Build and test**, then build this installer. Run it as an administrator on Windows 10 or Windows 11 x64. The driver and generated installer are restricted to internal distribution pending separate SIMCom redistribution approval.

Setup installs `modem-app.exe` and `modemd.exe` under the selected Program Files directory. Before registering the service, it invokes the native x64 `pnputil.exe` once for `simfilter.inf` and then once for `simser.inf`; each invocation stages the package in the Windows Driver Store and asks Plug and Play to bind it to a connected modem without forcing a downgrade from a newer or better-ranked driver. The modem may be disconnected during setup. A missing device or an already-selected newer driver is not an error; connect the modem later and Windows will use the staged packages. If Windows reports that a reboot is required, setup marks the installation for reboot. A process-launch failure is reported separately from a genuine driver rejection, which aborts setup before service registration and identifies the INF, `pnputil` exit code, and `%WINDIR%\inf\setupapi.dev.log`.

After staging the drivers, setup creates or canonicalizes `A7670ModemService` as `NT AUTHORITY\LocalService`, configures delayed automatic startup and 5/15/60-second recovery restarts, and verifies that the service starts.

Reinstalling or upgrading stops the service before replacing `modemd.exe`, stages the drivers without forcing an older version, reapplies the service's canonical configuration, and starts the bundled version. Normal uninstall stops and deletes the service before removing the application binaries and bundled driver source files. It deliberately leaves the published driver packages in the Windows Driver Store because connected hardware may still use them. Installation, upgrade, and uninstall always retain `%ProgramData%\A7670 Modem`, including the SQLite database, settings, history, integration secrets, and pending webhook records.

Verify driver staging and the AT port from an elevated PowerShell prompt:

```powershell
pnputil.exe /enum-drivers /class Ports
pnputil.exe /enum-devices /connected /deviceid 'USB\VID_1E0E&PID_9011'
Get-CimInstance Win32_SerialPort | Where-Object PNPDeviceID -like 'USB\VID_1E0E&PID_9011*'
Get-Service A7670ModemService
```

If staging fails, inspect `%WINDIR%\inf\setupapi.dev.log`, confirm the OS is x64 Windows 10/11, and run `pnputil.exe /add-driver "$env:ProgramFiles\A7670 Modem\drivers\simcom\simfilter.inf" /install` from an elevated prompt to reproduce the filter-package error. Repeat with `simser.inf` only after the filter package succeeds. Do not edit the vendor files: `scripts\validate-simcom-driver.ps1 -SourceZip D:\path\to\Windows10.zip` verifies the source archive, required layout, individual hashes, hardware IDs, and catalog signatures.

## Standalone service fallback

Use the scripts below only for development or troubleshooting when the combined NSIS installer cannot be used. Build on a compatible Windows x64 machine, copy `target\release\modemd.exe` and the scripts to the destination as needed, then open **PowerShell as Administrator**:

```powershell
# If modemd.exe and install-service.ps1 were copied to C:\Install
Set-Location C:\Install
.\install-service.ps1 -Binary .\modemd.exe

# Or, from a full source checkout after a local release build
.\scripts\install-service.ps1
```

The fallback installer copies the daemon to `C:\Program Files\A7670 Modem\modemd.exe`, creates or updates the same service configuration, and starts it. All service-control operations are checked and bounded by the same 30-second timeout used by the NSIS package.

Verify the deployment:

```powershell
Get-Service A7670ModemService
Test-Path "$env:ProgramData\A7670 Modem\modemd.sqlite3"
```

The second command confirms that the service created its SQLite data file; inspect its contents with a SQLite client. To stop and unregister only the service while retaining both application binaries and data:

```powershell
.\scripts\uninstall-service.ps1
```

The fallback uninstaller deliberately does not delete the combined Tauri installation directory. Use `.\scripts\uninstall-service.ps1 -PurgeData` only when intentionally deleting all local modem history, settings, integration secrets, audio metadata, REST communications, and pending webhook records. The normal NSIS uninstaller never purges this data.

## REST and webhook integration

The service optionally exposes `POST /api/v1/communications` and `GET /api/v1/health` on `0.0.0.0:5069` by default. Enable REST and set its bearer token through Settings; REST cannot be enabled without a token, and both endpoints require `Authorization: Bearer <configured-token>`. The communications endpoint accepts immediate `sms` and `call` requests with a non-empty opaque `request_id` and replays the originally reserved communication for duplicate request IDs.

The health endpoint returns the JSON primitive `true` with `200 OK` when the periodic modem probe reports Ready and the newest classified outbound SMS/call attempt from the last 24 hours does not indicate a modem or dispatch failure. It returns `false` with `503 Service Unavailable` otherwise. An idle Ready modem is healthy; in-progress attempts are neutral, and recipient outcomes such as SMS delivery failure, busy, or no-answer do not make the daemon unhealthy. The response never includes port names, destinations, errors, or other diagnostics.

Only REST-created communications create webhooks. Webhook payloads are deliberately minimal: event type plus `id`, `request_id`, and nullable `failure_reason`. This is intended for a firewall-restricted trusted LAN: HTTP does not protect bearer tokens or communication data in transit. Use network isolation or a TLS-terminating reverse proxy when needed. A REST bind-address change takes effect after restarting the service; other integration settings take effect immediately.

Set `MODEMD_INTEGRATION_DEBUG=1` in the service environment and restart it to enable short-lived sanitized Diagnostics data (at most 200 in-memory events). It never includes tokens, headers, phone numbers, message bodies, raw JSON/PDU, response text, or URL query values.

## Hardware acceptance notes

Before production rollout, record the A7670 model, COM port, `ATI`, `AT+CGMR`, and the raw behavior of relevant commands/prompts. For delivery reports also record `AT+CNMI=?`, `AT+CNMI?`, `AT+CSMP?`, and `AT+CPMS?`; test reachable/unreachable recipients, a service restart before receipt, a receipt during a call, and message-reference reuse. See [a7670c-sms-send-delivery-status.md](a7670c-sms-send-delivery-status.md) and [a7670c-automation-plan (2).md](a7670c-automation-plan%20(2).md) for the acceptance context.
