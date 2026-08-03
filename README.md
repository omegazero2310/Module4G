# A7670 Modem

Windows 10/11 x64 software for a single SIMCom A7670C-LANS module. The repository is a Rust
workspace with a protobuf contract, daemon core, deterministic simulator, and a Tauri 2 UI.

## Development

Prerequisites are Rust stable, Node.js 20+, the MSVC C++ build tools, and WebView2. `protoc` is
vendored by the Rust build. Run:

```powershell
cargo test --workspace
cargo run -p modem-sim
cd modem-app
npm.cmd install
npm.cmd run build
```

On Windows the simulator listens on `\\.\pipe\a7670-modemd-v1`. Start it before `tauri dev`; the
app then displays a simulated ready modem. It accepts a small deterministic line protocol used by
the development UI, plus core AT commands for parser development without physical hardware.

## Architecture

`modemd` is the sole serial-port owner. The intended production transport is tonic gRPC on
`\\.\pipe\a7670-modemd-v1`; browser code never accesses IPC directly. `modem-proto` defines the
versioned API, and the Tauri backend translates calls and events for React.

Production data belongs under `%ProgramData%\A7670 Modem\` and must be ACLed to LocalService and
administrators. Logs must redact phone numbers and exclude message bodies, audio, and unrestricted
AT output. The service scripts require an elevated PowerShell and preserve ProgramData unless
`uninstall-service.ps1 -PurgeData` is explicitly used.

## Current implementation status

Implemented: protobuf surface, byte-oriented framing including bare prompts, SMS/number limits,
UCS2 encoding, audio signature/size checks, guarded AT validation, settings defaults/validation,
Windows COM enumeration with VID/PID filtering, plug-and-play reconnect monitoring, AT-port
probing and modem initialization,
simulator responses, a Tauri navigation/dashboard shell, and service lifecycle scripts.

Still required before production use: SCM control-handler integration, named-pipe server and DACL,
serial command scheduling, SQLite migrations, complete feature state machines, Tauri command
coverage, bundling the release daemon into NSIS with custom upgrade rollback, and security-context
tests. The daemon is deliberately not declared as a Tauri resource yet, so `tauri dev` does not
depend on a prebuilt `target/release/modemd.exe`.

Physical acceptance is intentionally pending. With hardware, record actual composite COM
interfaces and raw prompt bytes; confirm VID/PID and baud; tune upload pacing; then exercise SMS
send/receive/delivery, USSD, answered and failed calls, remote audio, USB removal, and reconnect.
