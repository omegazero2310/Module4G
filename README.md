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

The daemon treats modem SMS storage as a durable inbox queue. Synchronization commits every
message and status report to SQLite before deleting those exact modem slots. This keeps a full `SM`
store from blocking inbound SMS or stored delivery reports while preserving the application archive.

## REST and webhook integration

The Windows service can expose `POST /api/v1/communications` on the configurable REST listener
(default `0.0.0.0:5069`). REST is disabled by default and cannot be enabled until a bearer token has
been stored on the Settings page. Requests support `sms` and `call` channels and immediate work
only. Each request must include a non-empty opaque `request_id`; it is unique for duplicate
prevention, while the daemon generates the UUID returned as `data.id`. The POST response also echoes
`data.request_id` and uses `data.contact` for the normalized recipient. Call content is the
case-insensitive name of an uploaded AMR-NB file; empty
call content uses the currently selected AMR file and returns validation error if none is selected.

Only REST-created communications produce lifecycle webhooks. Delivery uses a durable SQLite outbox,
preserves event order for each communication, disables redirects, and retries until a 2xx response.
The default receiver is `http://10.1.11.117:5068/api/v1/webhooks/receive`; its bearer token is
configured separately and is never returned to the UI.
Their payloads contain only the daemon `id`, caller `request_id`, and nullable `failure_reason`.

This interface is intended only for a firewall-restricted, trusted LAN. Plain HTTP does not encrypt
REST or webhook bearer tokens, phone numbers, or content in transit. Use network isolation or a TLS
terminating reverse proxy when the LAN cannot be fully trusted. Changing the bind address requires a
service restart; enablement, webhook URL, and token changes apply immediately.

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

For SMS delivery-report acceptance, record the A7670 model, COM port, `ATI`, `AT+CGMR`,
`AT+CNMI=?`, `AT+CNMI?`, `AT+CSMP?`, and `AT+CPMS?`. Test reachable and unreachable recipients,
daemon restart and application closure before the receipt, a receipt arriving during a call, and
TP-MR reuse. A missing carrier receipt is not a delivery failure. When delivery tracking was
verified, a submitted message remains Delivery pending for the `AT+CSMP=49,167,0,0` 24-hour
validity period, then becomes Delivery unknown if no terminal report arrives. A late terminal
report must still replace Delivery unknown. Confirm that an airplane-mode recipient releases Send
as soon as `+CMGS` and `OK` arrive; submission must never wait for handset delivery. If the modem
provides no final submission result within 40 seconds, the operation must finish as Send result
unknown.
