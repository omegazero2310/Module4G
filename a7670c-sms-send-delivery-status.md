# A7670C-LANS — SMS Send & Delivery Status (AT Command Reference)

Source: `A76XX Series_AT Command Manual_V1.06`, §9.1–9.2 (SMS commands), plus GSM 03.40 (3GPP TS 23.040) for PDU-level encoding the SimCom manual doesn't spell out itself.

Two independent things need checking: **(1) was it accepted by the network**, **(2) was it delivered to the handset**. They use different mechanisms and arrive at different times — sometimes seconds, sometimes minutes apart.

---

## 1. Send confirmation — identical in both modes

`AT+CMGS` (§9.2.13, p.225):

- Success → `+CMGS: <mr>` then `OK`
- Failure → `+CMS ERROR: <err>` or `ERROR`

**`<mr>`** = TP-Message-Reference, an 8-bit integer (0–255) the module assigns to this outgoing message. This is the single most important value to hold onto — it's the only thing that ties your later delivery report back to *this specific send*. Store it keyed by whatever your daemon uses to track the outbound message (DB row id, queue entry, etc).

This response only confirms the module handed the message to the SMSC (its local cell tower's message center). It says nothing about whether the recipient's phone ever saw it.

---

## 2. Text mode (AT+CMGF=1) — recap

```
AT+CSMP=49,167,0,0      // fo=49 requests a status report (default fo=17 does not)
AT+CNMI=2,1,0,1,0       // ds=1 -> status reports routed to TE as +CDS
AT+CMGS="+84901234567"
> Hello<Ctrl-Z>
+CMGS: 46
OK
...
+CDS: 6,46,"+84901234567",145,"26/08/04,14:32:10+28","26/08/04,14:32:12+28",0
```

`+CDS: <fo>,<mr>,[<ra>],[<tora>],<scts>,<dt>,<st>` — the module parses everything for you already. What each means:

| Param | Meaning |
|---|---|
| `<fo>` | First octet of the report PDU (49 = status report present in text mode framing) |
| `<mr>` | Same message reference as the original `+CMGS: <mr>` — **your correlation key** |
| `<ra>` | Recipient address — the number the SMS actually went to |
| `<tora>` | Type of `<ra>` address (145 = international format, i.e. `+`-prefixed E.164) |
| `<scts>` | Service-center timestamp — when the SMSC accepted your submission |
| `<dt>` | Discharge time — when the SMSC got confirmation the handset received it |
| `<st>` | Status byte — the actual result (0 = delivered, see §5 below) |

---

## 3. PDU mode (AT+CMGF=0) equivalents

### 3.1 Switch mode
```
AT+CMGF=0
```

### 3.2 Send command
```
AT+CMGS=<length>
<pdu-hex-string><Ctrl-Z>
```
- `<length>` = length of the **TP data unit in octets only**. The leading SMSC-address field is **not** counted (confirmed in manual §9.2.12's `<length>` definition, which applies identically to CMGS/CMGR/CDS).
- Response: same as text mode.

### 3.3 Requesting a status report — you set the bit yourself

In PDU mode there's no `AT+CSMP fo=49` shortcut — the **TP-SRR bit** lives inside the first octet byte you build by hand.

**SMS-SUBMIT first octet — bit by bit:**

| Bit(s) | Field | What it controls |
|---|---|---|
| 0–1 | TP-MTI (Message Type Indicator) | `00`=SMS-DELIVER-REPORT, `01`=**SMS-SUBMIT** (what you send), `10`=SMS-COMMAND, `11`=reserved |
| 2 | TP-RD (Reject Duplicates) | `1` = SMSC should reject if it already has an identical unacknowledged submission from you. Almost always `0`. |
| 3–4 | TP-VPF (Validity Period Format) | `00`=no VP field present, `10`=relative format (1 byte, most common), `01`/`11`=enhanced/absolute (rarely used) |
| 5 | **TP-SRR (Status Report Request)** | **`1` = you want a delivery report. `0` = you don't.** This is the bit that matters for this whole doc. |
| 6 | TP-UDHI (User Data Header Indicator) | `1` = message body starts with a header (used for concatenated/multipart SMS, custom ports). `0` for plain text. |
| 7 | TP-RP (Reply Path) | Rarely used; leave `0` |

Two values you'll actually use:
- `0x11` = SUBMIT + relative VP, **no** status report
- `0x31` = SUBMIT + relative VP, **with** status report (`0x11 | 0x20`)

### 3.4 Full SMS-SUBMIT PDU byte layout

| Field | Size | Meaning |
|---|---|---|
| SMSC address | 1+N bytes | Length byte, then type-of-address byte + BCD number. **Send length byte `00`** to tell the module "use the SMSC from `AT+CSCA`" — this avoids having to encode the SMSC address yourself. |
| First octet | 1 byte | See §3.3 table above |
| TP-MR | 1 byte | Message reference. Send `00` — the module assigns the real value and returns it in `+CMGS: <mr>` |
| TP-DA | 2+N bytes | Destination address — see breakdown below |
| TP-PID | 1 byte | Protocol Identifier. `00` = normal SMS (no special telematic interworking). Leave `00` unless you have a specific reason not to. |
| TP-DCS | 1 byte | Data Coding Scheme — controls character set. `00` = GSM 7-bit default alphabet. `08` = UCS2 (needed for non-Latin scripts, emoji, etc — costs you message length, ~70 chars max per part instead of ~160) |
| TP-VP | 1 byte (only if VPF≠`00`) | Validity period — how long the SMSC should keep retrying before giving up. See table below. |
| TP-UDL | 1 byte | User Data Length — septet count if 7-bit encoded, octet count if 8-bit/UCS2 |
| TP-UD | N bytes | The message body itself, packed |

**TP-DA (destination address) breakdown:**

| Sub-field | Meaning |
|---|---|
| Length byte | Number of **decimal digits** in the number (not byte count) |
| Type-of-address byte | Bit 7 always `1`. Bits 6–4 = Type of Number (`001`=international, `000`=unknown/local). Bits 3–0 = Numbering Plan (`0001`=ISDN/telephone/E.164). `0x91` = international E.164 (leading `+` present); `0x81` = unknown type, still E.164 numbering plan (no `+`). This matches what the manual states for `<toda>`: default `145` (`0x91`) when the number starts with `+`, default `129` (`0x81`) otherwise. |
| Digits | BCD-packed, **nibble-swapped in pairs**: for digits `d1 d2 d3 d4...`, byte1 = `(d2<<4)\|d1`, byte2=`(d4<<4)\|d3`, etc. Odd digit count → pad the final nibble with `F`. |

**TP-VP relative validity period table** (only relevant when VPF=`10`):

| VP byte range (decimal) | Meaning |
|---|---|
| 0–143 | `(VP+1) × 5` minutes (5 min – 12 hours) |
| 144–167 | 12 hours + `(VP−143) × 30` minutes (12.5 – 24 hours) |
| 168–196 | `(VP−166) × 1` day (2 – 30 days) |
| 197–255 | `(VP−192) × 1` week (5 – 63 weeks) |

`0xAA` = 170 decimal → falls in 168–196 range → `170−166 = 4` days.

**TP-UD (7-bit packing):** GSM 7-bit default alphabet characters are 7 bits each. They get packed into 8-bit octets with no gaps — each octet borrows bits from the next character, which is why 8 septets fit into exactly 7 octets. You don't need to hand-roll this; use a library, but the algorithm is: concatenate all 7-bit values into one continuous bitstream (character 0's bits first), then re-slice that stream into 8-bit bytes.

### 3.5 Worked, verified example — sending "Hi" to +84901234567 with a status report requested

```
AT+CMGF=0
AT+CMGS=16
0031000B914809214365F70000AA02C834
```

Breaking that hex string down byte by byte:

| Bytes | Value | Meaning |
|---|---|---|
| `00` | SCA length = 0 | Use default SMSC from `AT+CSCA` |
| `31` | First octet | SUBMIT, relative VP, **SRR=1** (status report requested) |
| `00` | TP-MR | Let module assign |
| `0B` | DA length | 11 digits |
| `91` | DA type | International (`+`-format) |
| `48 09 21 43 65 F7` | DA digits | Nibble-swapped `84901234567` |
| `00` | TP-PID | Normal |
| `00` | TP-DCS | GSM 7-bit |
| `AA` | TP-VP | 4 days |
| `02` | TP-UDL | 2 septets |
| `C8 34` | TP-UD | Packed "Hi" |

`AT+CMGS=16` — 16 is the TPDU length (everything **after** the `00` SMSC byte): `31 00 0B 91 48 09 21 43 65 F7 00 00 AA 02 C8 34` = 16 bytes.

This was built and independently checked with a small Python script (nibble-swap + 7-bit packing implemented directly, not hand-arithmetic) — the digit encoding and packing algorithms are exact, not approximated.

### 3.6 Enabling delivery reports — identical to text mode
```
AT+CNMI=2,1,0,1,0
```
`<ds>` is mode-independent; only the **shape of the URC that follows** changes.

### 3.7 Reading the delivery report in PDU mode
```
+CDS: <length>
<pdu-hex-string>
```
Same `<length>` rule — TP data unit octets only, SMSC field excluded.

**SMS-STATUS-REPORT byte layout (network → your module):**

| Field | Size | Meaning |
|---|---|---|
| SMSC address | 1+N | Usually not needed for your logic — skip past it using its length byte |
| First octet | 1 byte | Bits 0–1 = `10` marks this as a status report. Other bits (TP-MMS "more messages to send", TP-SRQ) are rarely needed for basic tracking. |
| TP-MR | 1 byte | **Echoes the `<mr>` from your original `+CMGS`** — match this against your pending-message table |
| TP-RA | 2+N | Recipient address — same length/type/nibble-swap encoding as TP-DA |
| TP-SCTS | 7 bytes | Service-center timestamp: year, month, day, hour, minute, second, timezone — each a 2-digit decimal value, nibble-swapped the same way as address digits (e.g. "26" → swap → `62`) |
| TP-DT | 7 bytes | Discharge time — when the SMSC got final confirmation. Same 7-byte format as SCTS. `DT − SCTS` roughly tells you how long delivery took. |
| TP-ST | 1 byte | **The result you actually care about** — see §5 |

### 3.8 Worked, verified example — decoding a delivery report for the message above

Continuing the same scenario (module assigned `<mr>=46`, delivered at 14:32:10→14:32:12 on 2026-08-04, timezone +7:00):

```
+CDS: 25
00062E0B914809214365F7628040412301826280404123218200
```

| Bytes | Value | Meaning |
|---|---|---|
| `00` | SCA length = 0 | (skip) |
| `06` | First octet | Status report marker |
| `2E` | TP-MR | `46` decimal — **matches our `+CMGS: 46`** |
| `0B 91 48 09 21 43 65 F7` | TP-RA | Same number, same encoding as TP-DA |
| `62 80 40 41 23 01 82` | TP-SCTS | `26/08/04 14:32:10 +7:00` (each byte is a nibble-swapped 2-digit value; last byte `82`→swap→`28`, ×15 min = 420 min = 7h) |
| `62 80 40 41 23 21 82` | TP-DT | `26/08/04 14:32:12 +7:00` |
| `00` | TP-ST | `0x00` = **delivered successfully** |

Timestamp decoding rule: each byte is one nibble-swapped 2-digit field — reverse the same operation used for the address digits (swap the two hex nibbles, read as decimal). Timezone is in units of 15 minutes from GMT; a negative offset sets bit 3 of the tens-digit nibble before swapping (rare in practice — most carriers report local time relative to GMT positively or your module normalizes it).

---

## 4. Address, PID and DCS — deeper meaning for troubleshooting

- **Type-of-address `0x91` vs `0x81`**: if your daemon always sends E.164 numbers with a `+`, you'll always encode `0x91`. If a recipient number arrives without `+` (e.g. a local 10-digit number), decide up front whether to normalize it to E.164 before encoding, or use `0x81`/unknown-type — mixing conventions is a common source of "message accepted but never delivered" bugs.
- **TP-PID `0x00`**: means "this is a normal short message, no special handling." Non-zero values exist for things like SIM data download, telex, fax — you won't need them for a chat/notification use case.
- **TP-DCS**: bits 7–6 = `00` for the "general data coding" group used here. Bits 5–4 select the alphabet: `00`=GSM 7-bit, `01`=8-bit data, `10`=UCS2. Bit 4=`1` additionally signals "message class present" in bits 1–0 (class 0 = flash/display-immediately, class 1 = ME-specific storage, etc) — most applications leave this unset (`0x00` / `0x08`).

---

## 5. TP-Status (`<st>`) byte — not in the SimCom manual, from 3GPP TS 23.040 §9.2.3.15

The A76XX manual only says *"GSM 03.40 TP-Status in integer format, 0…255"* — no table. In practice:

| Value | Meaning |
|---|---|
| `0x00` | Delivered successfully |
| `0x01`–`0x02` | Forwarded to / replaced by another SC — treat as informational, not a hard confirmation |
| `0x20`–`0x25` | Temporary error — SC is still retrying. Keep waiting. |
| `0x40`–`0x45` | Permanent error — SC gave up. Treat as failed. |
| `0x60`–`0x65` | Temporary error, but SC has also stopped retrying — effectively failed even though it's "temporary" |

For a daemon: `0x00` → delivered. `0x20–0x25` → still pending, don't mark failed yet. Anything else → failed.

---

## 6. Practical recommendation

Text mode already parses `<fo>/<mr>/<ra>/<scts>/<dt>/<st>` for you — zero PDU decoding needed. PDU mode is only *required* when the message body needs UCS2 (non-Latin text) or concatenated/multipart SMS, which text mode can't express cleanly.

A common daemon design: stay in **text mode** for control/status parsing, and switch to **PDU mode only** around the `AT+CMGS` call when the body needs UCS2 or multipart encoding — then switch back. Keeps your `+CDS` handling code simple while still supporting Unicode messages when needed. If you do go full-PDU for everything, the byte tables and verified examples above are what to implement/test against.
