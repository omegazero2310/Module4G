# A7670C-LANS Automation — System Plan

Manual verified against: `A76XX-Series_AT_Command_Manual_V1_06-4.pdf`

## 1. Architecture

```
┌─────────────────────┐        gRPC (TCP 127.0.0.1:xxxx or UDS)       ┌──────────────────────┐
│   Client App(s)      │ ───────────────────────────────────────────► │   modemd (daemon)     │
│  Tauri / CLI / MAUI  │ ◄─────────────────────────────────────────── │   Rust, tokio          │
└─────────────────────┘        streamed events (call state, SMS)      └──────────┬────────────┘
                                                                                    │ serial (AT port)
                                                                                    ▼
                                                                          SIMCom A7670C-LANS
```

**Why split daemon/app:** the modem is a single half-duplex AT command channel — only one process can own the serial port. The daemon is the sole owner; it serializes commands (queue, one in-flight at a time, matched to response/URC), and multiple UIs can attach as clients without contention. The daemon can run headless (systemd service, Windows service, Docker container) independent of whatever UI is used.

## 2. Language choice

**Daemon: Rust.**
- `serialport-rs` — one API for `/dev/ttyUSB*` (Linux) and `COM*` (Windows), no per-platform serial code.
- USB enumeration by VID:PID (to *find* the module, not guess a port number) via `nusb` / `rusb`.
- Compiles to a single static binary (musl on Linux) — trivial minimal Docker image, no runtime to install.
- C# (.NET 8 AOT) is technically viable for the daemon, but USB device discovery by VID:PID cross-platform is weaker than Rust's ecosystem, and a slim Docker-friendly background service is more natural in Rust.

**Client app: Tauri (Rust + webview).** Cross-platform, small, speaks gRPC to the daemon using shared generated types from one `.proto`. A MAUI client is also viable later — the IPC design doesn't lock you into one client language.

**IPC: gRPC (tonic on the Rust side).**
- Same contract serves a Rust/Tauri client, a future MAUI client, or a CLI — no protocol reimplementation per language.
- Identical over TCP loopback on Windows/Linux, so Docker is trivial.
- Native server-streaming support — needed for pushing call-state and incoming-SMS events to the UI in real time instead of polling.

## 3. Docker note

USB passthrough works cleanly on **Linux Docker** with `--device=/dev/ttyUSB0` (the A7670C exposes several USB serial interfaces — AT, PPP/modem, diag; identify the right one by VID:PID + interface number). On **Windows**, containers don't do USB passthrough directly — use `usbipd-win` to attach the USB device into WSL2 first, then pass it into the Linux container from there. Practical default: daemon runs natively as a Windows service on Windows hosts, and inside Docker only on Linux hosts.

## 4. Per-feature command plan (verified against manual)

### 4.1 Send SMS
```
AT+CMGF=1
AT+CSCS="GSM"                  # or "UCS2" for Vietnamese diacritics
AT+CMGS="+849xxxxxxxx"
<message text><Ctrl-Z>
```
Reference: §9.2.3, §9.2.13

### 4.2 SMS status → log
```
AT+CSMP=49,167,0,0             # fo=49 requests a delivery status report in text mode
AT+CNMI=2,1,0,1,0              # mt=1 → +CMTI on new incoming SMS, ds=1 → +CDS delivery reports pushed directly
```
- Incoming SMS: `+CMTI: "SM",<index>` → read with `AT+CMGR=<index>`.
- Delivery report: `+CDS: <fo>,<mr>,[<ra>],[<tora>],<scts>,<dt>,<st>` (text mode).
- Daemon writes every send attempt + resulting `+CDS` (or timeout) to SQLite: timestamp, number, message ref, status.

Reference: §9.2.6, §9.2.9

### 4.3 TTS → AMR → onboard file → playback into call
- TTS/encode to AMR is done client-side (ffmpeg + `libopencore-amrnb`), not on the module.
- Upload to module filesystem:
```
AT+CFTRANRX="c:/msg.amr",<len>
> <stream len bytes>
OK
```
- To replace an existing file, delete first: `AT+FSDEL="c:/msg.amr"`, then re-upload.
- Playback into the **active call**:
```
AT+CCMXPLAY="c:/msg.amr",1,0   # play_path=1 = remote path (voice call), repeat=0 = play once
```
- URCs mark playback boundaries: `+AUDIOSTATE: audio play` / `+AUDIOSTATE: audio play stop`.
- Stop early: `AT+CCMXSTOP`.
- This plays directly into the call audio path — no need for the CM108 analog injection card on this module; switch over for cleaner audio and one less physical component.

Reference: §13.2.1, §12.2.5 (FSDEL), §21.2.1, §21.2.2

### 4.4 Make call, poll status, playback, hang up
Startup (once):
```
AT+CVHU=0                      # REQUIRED — without this, ATH is silently ignored (returns OK, doesn't hang up)
AT+CLCC=1                      # push +CLCC automatically on every call state change (avoids polling loop)
```
Dial:
```
ATD+849xxxxxxxx;
```
Outcomes:
- `OK` + `VOICE CALL:BEGIN` → answered → `AT+CCMXPLAY="c:/msg.amr",1,0`
- `BUSY` / `NO ANSWER` / `NO CARRIER` (final result codes, not just NO CARRIER) → immediately `AT+CEER` for a human-readable cause (e.g. `"17 User busy"`, `"19 User alerting: no answer"`) → log it
- After playback: `+AUDIOSTATE: audio play stop` → `ATH` (works because CVHU=0 was set at startup)
- `VOICE CALL: END: <time>` → log the call duration string given for free by the module

Reference: §2.2.1 (ATD), §2.2.3 (ATH), §7.2.6 (CLCC), §7.2.7 (CEER), §28.1–28.2 (result codes / CEER cause table)

### 4.5 Balance check
```
AT+CUSD=1,"*101#"
```
- Response: `+CUSD: <m>,<str>,<dcs>` — decode `<str>` per `<dcs>` (17 default = GSM-7 packed; other values may be UCS2).
- USSD reply format is carrier-specific (Viettel/Mobifone/Vinaphone differ) — needs a small per-carrier parser to extract the balance figure and compare against a configured low-balance threshold.
- Note: the third `AT+CUSD` parameter is `<dcs>`, not a timeout — there is no timeout parameter on this command.

Reference: §4.2.3

## 5. Data model

Three layers: the gRPC contract clients see, the daemon's internal command-queue/URC types, and the SQLite log schema.

### 5.1 `.proto` — client-facing contract

```protobuf
syntax = "proto3";
package modemd.v1;

service Modem {
  rpc SendSms (SendSmsRequest) returns (SendSmsResponse);
  rpc ListSms (ListSmsRequest) returns (ListSmsResponse);

  rpc UploadAmr (stream UploadAmrChunk) returns (UploadAmrResponse);

  rpc MakeCall (MakeCallRequest) returns (MakeCallResponse);
  rpc HangUp (HangUpRequest) returns (Empty);
  rpc ListCalls (ListCallsRequest) returns (ListCallsResponse);

  rpc CheckBalance (Empty) returns (BalanceResponse);

  // single stream, server pushes everything as it happens —
  // call state changes, SMS delivery reports, incoming SMS, low-balance warnings
  rpc StreamEvents (Empty) returns (stream Event);

  rpc GetModemStatus (Empty) returns (ModemStatus);
}

// ---- SMS ----
message SendSmsRequest { string number = 1; string text = 2; }
message SendSmsResponse { string message_ref = 1; }

message ListSmsRequest { int64 since_unix = 1; uint32 limit = 2; }
message ListSmsResponse { repeated SmsRecord records = 1; }

message SmsRecord {
  string id = 1;
  string number = 2;
  string text = 3;
  SmsDirection direction = 4;
  SmsStatus status = 5;
  string cause = 6;          // e.g. delivery failure reason, empty if n/a
  int64 sent_unix = 7;
  int64 status_updated_unix = 8;
}
enum SmsDirection { SMS_OUT = 0; SMS_IN = 1; }
enum SmsStatus { SMS_PENDING = 0; SMS_DELIVERED = 1; SMS_FAILED = 2; SMS_UNKNOWN = 3; }

// ---- AMR upload ----
message UploadAmrChunk { string filename = 1; bytes data = 2; bool last = 3; }
message UploadAmrResponse { string filepath = 1; uint32 bytes_written = 2; }

// ---- Calls ----
message MakeCallRequest { string number = 1; string amr_filepath = 2; }
message MakeCallResponse { string call_id = 1; }
message HangUpRequest { string call_id = 1; }

message ListCallsRequest { int64 since_unix = 1; uint32 limit = 2; }
message ListCallsResponse { repeated CallRecord records = 1; }

message CallRecord {
  string id = 1;
  string number = 2;
  CallOutcome outcome = 3;   // ANSWERED / BUSY / NO_ANSWER / NO_CARRIER / ERROR
  string cause = 4;          // AT+CEER text when outcome != ANSWERED
  uint32 duration_sec = 5;   // from "VOICE CALL: END: <time>"
  int64 started_unix = 6;
  int64 ended_unix = 7;
}
enum CallOutcome { CALL_UNKNOWN = 0; CALL_ANSWERED = 1; CALL_BUSY = 2; CALL_NO_ANSWER = 3; CALL_NO_CARRIER = 4; CALL_ERROR = 5; }

// ---- Balance ----
message BalanceResponse { string raw_ussd_reply = 1; optional double amount = 2; string currency = 3; int64 checked_unix = 4; }

// ---- Events (server push) ----
message Event {
  int64 unix = 1;
  oneof payload {
    CallStateChanged call_state_changed = 2;
    SmsStatusChanged sms_status_changed = 3;
    SmsReceived sms_received = 4;
    LowBalanceWarning low_balance = 5;
    ModemDisconnected modem_disconnected = 6;
  }
}
message CallStateChanged { string call_id = 1; CallOutcome outcome = 2; string detail = 3; }
message SmsStatusChanged { string message_id = 1; SmsStatus status = 2; }
message SmsReceived { SmsRecord record = 1; }
message LowBalanceWarning { double amount = 1; double threshold = 2; }
message ModemDisconnected { string reason = 1; }

message ModemStatus { bool connected = 1; string port = 2; string imei = 3; int32 signal_dbm = 4; }
message Empty {}
```

Design choices:
- **One `StreamEvents` call, not five** — a single tagged union keeps client reconnect/subscribe logic simple and matches how the UI renders it (one live log).
- **`UploadAmr` is client-streaming, not unary** — matches how `AT+CFTRANRX` wants the data (chunked, with backpressure), avoids holding a multi-hundred-KB file in one gRPC message.
- **IDs are strings (ULIDs), not the modem's own `<mr>`/`<id1>`** — those are small ints the modem reuses, not stable identifiers for the log.

### 5.2 Internal daemon types (not exposed over gRPC)

```rust
/// One item in the FIFO command queue — the modem is half-duplex,
/// only one AT command in flight ever.
struct PendingCommand {
    id: u64,
    raw: String,                           // e.g. "AT+CMGS=\"+849...\""
    payload: Option<Vec<u8>>,              // bytes to send once ">" prompt appears (CMGS text, CFTRANRX bytes)
    awaiting_prompt: bool,
    lines: Vec<String>,                    // accumulated response lines so far
    reply_tx: oneshot::Sender<AtResult>,
    timeout: Duration,
}

enum AtResult {
    Ok(Vec<String>),
    Error(String),
    CmeError(u32),
    CmsError(u32),
    TimedOut,
}

/// Parsed unsolicited codes — everything that isn't a reply to a command in flight.
enum Urc {
    VoiceCallBegin,
    VoiceCallEnd { duration_sec: u32 },
    Busy,
    NoAnswer,
    NoCarrier,
    Ring,
    CallListChanged(String),               // raw +CLCC line, parsed downstream
    IncomingSms { mem: String, index: u32 }, // +CMTI
    DeliveryReport(String),                // raw +CDS line, parsed downstream
    UssdReply(String),                     // raw +CUSD line, parsed downstream
    AudioPlayStarted,
    AudioPlayStopped,
}

/// The daemon's single source of truth for "what is the modem doing right now".
struct ModemSession {
    port: Box<dyn SerialPort>,
    cmd_queue: VecDeque<PendingCommand>,
    active_call: Option<CallState>,
    event_tx: broadcast::Sender<Urc>,      // fan-out to all connected gRPC StreamEvents clients
}

struct CallState {
    call_id: Ulid,
    number: String,
    amr_filepath: Option<String>,
    stage: CallStage,
    started: Instant,
}
enum CallStage { Dialing, Ringing, Answered, PlayingAudio, Ending }
```

### 5.3 SQLite schema (the log)

```sql
CREATE TABLE sms_log (
    id            TEXT PRIMARY KEY,       -- ULID
    number        TEXT NOT NULL,
    text          TEXT NOT NULL,
    direction     TEXT NOT NULL CHECK (direction IN ('out','in')),
    status        TEXT NOT NULL DEFAULT 'pending',
    cause         TEXT,
    message_ref   INTEGER,                -- modem's <mr>, for matching +CDS
    sent_unix     INTEGER NOT NULL,
    status_updated_unix INTEGER
);

CREATE TABLE call_log (
    id            TEXT PRIMARY KEY,       -- ULID
    number        TEXT NOT NULL,
    outcome       TEXT NOT NULL,          -- answered/busy/no_answer/no_carrier/error
    cause         TEXT,                   -- AT+CEER text
    duration_sec  INTEGER DEFAULT 0,
    amr_filepath  TEXT,
    started_unix  INTEGER NOT NULL,
    ended_unix    INTEGER
);

CREATE TABLE balance_log (
    id            TEXT PRIMARY KEY,
    raw_reply     TEXT NOT NULL,
    amount        REAL,
    currency      TEXT,
    checked_unix  INTEGER NOT NULL
);

CREATE INDEX idx_sms_sent ON sms_log(sent_unix);
CREATE INDEX idx_call_started ON call_log(started_unix);
```

`message_ref` on `sms_log` is the bridge between "what was sent" and the async `+CDS` that arrives later — the only place the modem's own small integer IDs need to survive past the request/response cycle.

## 6. Reader-task design

One task owns the serial port's read half. It has two jobs: split incoming bytes into command replies vs. URCs, and catch the bare `>` prompt (`AT+CMGS`/`AT+CFTRANRX`) that has no line terminator.

### 6.1 Line classification (URC vs. reply)

Each complete line is checked against a fixed set of URC prefixes (`VOICE CALL`, `+CMTI`, `+CDS`, `RING`, `BUSY`, `NO ANSWER`, `NO CARRIER`, `+CUSD`, `+AUDIOSTATE`, unsolicited `+CLCC`). If it matches, it's pushed to the `broadcast` event channel. Otherwise it's accumulated onto `cmd_queue.front_mut().lines` until a terminator (`OK` / `ERROR` / `+CME ERROR:` / `+CMS ERROR:`) closes out the command and completes its `oneshot`.

**Ambiguity to handle explicitly:** `NO CARRIER` / `BUSY` / `NO ANSWER` can be either the terminal result of a failed `ATD` in flight, or an unsolicited notice that an active call just dropped. Rule: if the command at the front of the queue is a dial command (`raw.starts_with("ATD")`), treat the line as that command's terminal result; otherwise treat it as a URC.

### 6.2 The `>` prompt problem

`AT+CMGS` and `AT+CFTRANRX` send the continuation prompt with **no newline** — `read_line()` can never catch it, since there's no delimiter to wait for. Fix: switch to raw byte reads (`AsyncReadExt::read`) instead of `read_line`, and treat **silence** as the completion signal rather than a delimiter:

1. **Fast path** — after draining every complete `\n`-terminated line each loop iteration, if the leftover (non-newline-terminated) tail is exactly `>` or `> `, handle it immediately as a prompt.
2. **Timeout path** — wrap each raw read in a short timeout (~200ms, needs tuning against actual hardware latency); if it elapses with zero new bytes and the buffer isn't empty, re-check the same trailing-prompt condition. Covers the case where the prompt byte arrives in its own read chunk with nothing following.

When a prompt is confirmed: pop the front queue entry's `payload`, mark `awaiting_prompt = false`, and hand the payload off to a separate writer task via an `mpsc` channel (`WriterCmd::SendPayload { bytes, terminator }`) — `terminator` is `CtrlZ` for `AT+CMGS` text, `None` for `AT+CFTRANRX`'s raw byte stream (length-bound instead).

**Open items to verify against real hardware before trusting this fully:**
- Exact prompt bytes — capture a raw hex dump (`xxd` on the serial port during a manual `AT+CMGS` test) to confirm it's really bare `>`/`> ` and not preceded by a stray `\r\n`.
- The 200ms timeout is a starting guess, not a spec value — tune down once round-trip latency on the actual USB link is known.
- Writer task (dequeue → write → append `Ctrl-Z` for CMGS) is the next piece to design — not yet sketched.

## 7. State machine summary

```
startup:      AT+CVHU=0
              AT+CLCC=1
              AT+CNMI=2,1,0,1,0

dial:         ATD+849xxxxxxxx;
   → OK + VOICE CALL:BEGIN         → answered: AT+CCMXPLAY="c:/msg.amr",1,0
   → BUSY / NO ANSWER / NO CARRIER → AT+CEER → log cause
   → +AUDIOSTATE: audio play stop  → ATH
   → VOICE CALL: END: <time>       → log duration
```

## 8. Next step — researched

### 8.1 Identifying the module over USB
Pulled the actual document — **A76XX Series_Linux_USB_Application Note_V1.00** (SIMCom, 2021.09.03). It confirms:

- **VID = `0x1e0e`, PID = `0x9011`** (`SIMCOM_VENDOR_ID` / `SIMCOM_PRODUCT_PID_X9011` in the note's kernel driver snippet).
- **Port layout** (once the `option` USB-serial driver binds all four interfaces):
  ```
  /dev/ttyUSB0   diag port     — developer/debug messages
  /dev/ttyUSB1   AT port       — AT commands (this is the one the daemon owns)
  /dev/ttyUSB2   Modem port    — ppp-dial / raw data
  /dev/ttyUSB3   NMEA port     — GNSS output (if GNSS is enabled)
  ```
  Note the doc's own changelog flags this port numbering isn't fixed across firmware/history — an earlier revision used `/dev/ttyUSB3` for what's now `/dev/ttyUSB2`, so **match by USB interface number via `serialport`'s `UsbPortInfo`, not by trusting `ttyUSB1` as a hardcoded path** — enumerate all four, open each, and confirm the AT port by sending a bare `AT` and checking for `OK` (the doc's own recommended test: `echo -e "at\r\n" > /dev/ttyUSBx`).
- On a stock kernel, the generic `option` driver usually already matches VID `0x1e0e` for other SimTech PIDs, but the doc's Linux distros may need PID `0x9011` explicitly added to `option.c` and rebuilt if the module isn't recognized out of the box — worth checking `dmesg | grep option` first before assuming a custom driver build is needed.
- This document was written for the general A76XX Linux/USB story, not audio playback or SMS, so it doesn't add anything to §4 — its only relevance here is confirming exactly what §8.1 needed.

### 8.2 Rust crate choices for the daemon skeleton
- **`serialport` (crates.io, latest 4.9.0)** — the standard cross-platform serial crate, exposes `available_ports()` with `UsbPortInfo` (VID/PID/serial number) for matching the module by identity rather than a fixed device path. Two things worth knowing before committing:
  - On Linux it depends on **libudev** by default for enumeration — the Docker image needs `libudev1` (runtime) or `libudev-dev` (build) installed, or the `libudev` cargo feature must be disabled (at the cost of less USB metadata, e.g. no VID/PID on some setups).
  - The project's own README currently states it's **looking for maintainers, especially for Windows** — not a blocker, but worth a mental note to pin a known-good version and not assume ongoing active maintenance.
  - A newer alternative, **`serialport5`**, offers the same functionality through a plain struct instead of `Box<dyn SerialPort>` — worth a look if starting fresh, since it avoids the trait-object indirection.
- **`tokio-serial`** wraps `serialport` for async read/write under tokio — this is what the reader/writer tasks sketched earlier assume.
- **`nusb`** (pure-Rust, no libusb dependency) for raw USB device enumeration by VID:PID if you want to detect the module before it's even bound as a serial device (e.g. to catch it during Linux driver attach, or for a "waiting for module..." UI state) — complementary to `serialport`, not a replacement.

### 8.3 Sequencing
1. Confirm real VID:PID and port layout on your actual A7670C-LANS unit (§8.1) — do this before writing any matching code, not after.
2. Daemon skeleton: USB enumeration/match → open port → command queue with URC dispatch (per §6) → SQLite log (§5.3) → gRPC server (§5.1) with one RPC per feature plus the streaming events RPC.
3. Writer task: dequeue `PendingCommand` → write bytes → handle the `>`-prompt payload branch (§6.2) → apply per-command timeout.
4. Client: thin once the daemon's contract is stable — Tauri app consuming the same generated protobuf types.
