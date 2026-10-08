# Calling the Viettel balance REST API

> Status: implemented in source. Rebuild and restart the daemon to use these endpoints. Physical Viettel SIM acceptance is still pending. Responses use the existing status/message/data envelope; examples below focus on data.

This API checks the Viettel SIM installed in the modem. A request makes the daemon send the existing `TK` query to `191` and wait for the carrier's reply. Sending the SMS successfully does not mean the balance is ready; verify this carrier profile on your SIM during hardware acceptance.

See the [implementation plan](docs/plans/2026-10-08-viettel-balance-rest-ui.md) for service, UI, persistence, and hardware acceptance work.

## Connection and authentication

Enable REST and configure a bearer token through the application's integration settings. The default listener bind address is `0.0.0.0:5069`; clients must use the service machine's actual IP address or hostname, or `127.0.0.1` when running on that machine. Bind-address changes require a service restart.

Every endpoint requires:

```http
Authorization: Bearer <your-configured-token>
```

Do not commit tokens or include them in logs. Plain HTTP exposes the token and data in transit; use an isolated trusted LAN or a TLS-terminating reverse proxy.

The examples below use PowerShell. Set `MODEMD_REST_TOKEN` in your process environment through your normal secret-management mechanism before running them. The scripts do not print the token.

```powershell
$modemApiBase = 'http://127.0.0.1:5069'
if ([string]::IsNullOrWhiteSpace($env:MODEMD_REST_TOKEN)) {
    throw 'Set MODEMD_REST_TOKEN to the configured REST bearer token.'
}
$modemApiHeaders = @{
    Authorization = 'Bearer ' + $env:MODEMD_REST_TOKEN
}
```

### cURL example (PowerShell)

Use `curl.exe` in PowerShell so the command invokes cURL rather than the PowerShell `curl` alias. Set the token in your process environment using your normal secret-management mechanism.

```powershell
$modemApiCurlAuth = 'Authorization: Bearer ' + $env:MODEMD_REST_TOKEN
```

Start a balance check with a unique request ID:

```powershell
$balanceRequestId = [guid]::NewGuid().ToString()
curl.exe -i `
    -X POST `
    "$modemApiBase/api/v1/balance-checks" `
    -H $modemApiCurlAuth `
    -H 'Content-Type: application/json' `
    --data-raw ('{"request_id":"' + $balanceRequestId + '"}')
```

The response includes `data.id`. Use that ID to retrieve the operation; poll this endpoint until its status is terminal:

```powershell
$balanceCheckId = '<id returned by the POST request>'
curl.exe -i `
    "$modemApiBase/api/v1/balance-checks/$balanceCheckId" `
    -H $modemApiCurlAuth
```

To read the latest stored balance without starting another check:

```powershell
curl.exe -i `
    "$modemApiBase/api/v1/balance" `
    -H $modemApiCurlAuth
```

## 1. Start a balance check

Endpoint: `POST /api/v1/balance-checks`.

Use a unique, non-empty `request_id` for each new check. Keep the same ID when retrying a request whose response was lost; generating a new ID for that retry could start another check.

```powershell
$balanceRequestId = [guid]::NewGuid().ToString()
$balanceRequestBody = @{
    request_id = $balanceRequestId
} | ConvertTo-Json -Compress

$balanceOperation = Invoke-RestMethod `
    -Method Post `
    -Uri "$modemApiBase/api/v1/balance-checks" `
    -Headers $modemApiHeaders `
    -ContentType 'application/json' `
    -Body $balanceRequestBody

$balanceCheckId = $balanceOperation.data.id
$balanceOperation
```

Initial response: `202 Accepted`.

```json
{
  "data": {
    "id": "d48a6952-8328-48c1-a2e1-04bb93d1439b",
    "request_id": "client-generated-request-key",
    "status": "queued",
    "created_at": "2026-10-08T08:00:00Z",
    "updated_at": "2026-10-08T08:00:00Z",
    "completed_at": null,
    "balance": null,
    "failure_reason": null
  }
}
```

The daemon owns the carrier SMS destination and query body. Clients do not submit a phone number, arbitrary SMS body, or AT command to this endpoint. The carrier query must be verified for the installed SIM's account type before the feature is enabled.

## 2. Retrieve the check result

Endpoint: `GET /api/v1/balance-checks/{id}`.

```powershell
$balanceOperation = Invoke-RestMethod `
    -Method Get `
    -Uri "$modemApiBase/api/v1/balance-checks/$balanceCheckId" `
    -Headers $modemApiHeaders

$balanceOperation
```

Successful response: `200 OK`.

```json
{
  "data": {
    "id": "d48a6952-8328-48c1-a2e1-04bb93d1439b",
    "request_id": "client-generated-request-key",
    "status": "succeeded",
    "created_at": "2026-10-08T08:00:00Z",
    "updated_at": "2026-10-08T08:00:18Z",
    "completed_at": "2026-10-08T08:00:18Z",
    "balance": {
      "amount_vnd": 125000,
      "observed_at": "2026-10-08T08:00:18Z"
    },
    "failure_reason": null
  }
}
```

Amounts and timestamps above are illustrative. `amount_vnd` is an integer number of Vietnamese dong. Zero is a valid balance; null means no balance was obtained for this operation. The balance represents a carrier observation at `observed_at`, rather than a continuously live value.

The service allows 120 seconds to schedule a queued request, then starts a 120-second reply window when dispatch is claimed. Delivery configuration and inbox preparation happen before that claim. The serial actor rejects an expired query before writing to the modem. Restart preserves the deadline and never automatically resends a possibly submitted query. Successful or explicitly failed checks impose a 60-second cooldown; timed-out checks impose a five-minute quarantine after their deadline. `request_id` accepts 1–256 bytes, must contain a non-whitespace character, and has a separate namespace from communication request IDs.

| Status | Client behavior |
| --- | --- |
| `queued` | Wait for modem scheduling. |
| `sending` | Wait for SMS submission to finish. |
| `waiting_reply` | Poll for a correlated carrier reply. |
| `send_unknown` | Submission is indeterminate; continue polling until resolved or timed out. Do not automatically send another query. |
| `succeeded` | Read `balance` and stop polling. |
| `failed` | Read the sanitized `failure_reason` and stop polling. |
| `timed_out` | No confirmed result within the service deadline; stop polling. |

Bounded polling example, using the ID returned from the start request:

```powershell
$balanceTerminalStates = @('succeeded', 'failed', 'timed_out')
$balancePollDeadline = [DateTimeOffset]::UtcNow.AddMinutes(3)

do {
    $balanceOperation = Invoke-RestMethod `
        -Method Get `
        -Uri "$modemApiBase/api/v1/balance-checks/$balanceCheckId" `
        -Headers $modemApiHeaders

    if ($balanceOperation.data.status -in $balanceTerminalStates) {
        break
    }

    Start-Sleep -Seconds 2
} while ([DateTimeOffset]::UtcNow -lt $balancePollDeadline)

if ($balanceOperation.data.status -eq 'succeeded') {
    $balanceOperation.data.balance
} elseif ($balanceOperation.data.status -in $balanceTerminalStates) {
    $balanceOperation.data | Select-Object status, failure_reason
} else {
    Write-Warning 'Client stopped waiting. Keep the check ID and retrieve its status later; do not automatically start another check.'
}
```

The three-minute limit is an example client wait budget, not the service's reply timeout. A client timeout does not cancel the durable operation. Production clients should back off on transient network/server errors and respect `Retry-After`.

## 3. Read the latest stored balance

Endpoint: `GET /api/v1/balance`. This reads the latest successful snapshot without sending another SMS.

```powershell
$latestBalance = Invoke-RestMethod `
    -Method Get `
    -Uri "$modemApiBase/api/v1/balance" `
    -Headers $modemApiHeaders

$latestBalance
```

Response: `200 OK`.

```json
{
  "data": {
    "balance": {
      "amount_vnd": 125000,
      "observed_at": "2026-10-08T08:00:18Z"
    },
    "freshness": "fresh",
    "active_check_id": null,
    "retry_after_seconds": 0
  }
}
```

Freshness values are `fresh`, `stale`, and `unavailable`. A successful snapshot is fresh for five minutes; a later failed or timed-out refresh also makes it stale. Before the first successful check, return `balance: null` with `freshness: "unavailable"`. A failed refresh preserves the last successful snapshot and its original observation time. If the SIM changes, the previous SIM's balance must not be presented as current.

## Errors and retry behavior

Errors use the existing status/message/data envelope. Authentication is checked before reading a request body.

| Condition | Expected behavior |
| --- | --- |
| Missing/invalid bearer token or REST disabled | `401 Unauthorized`. |
| Invalid JSON or empty `request_id` | `400 Bad Request` for malformed JSON; `422 Unprocessable Entity` for an invalid shape, unknown fields, or invalid request_id; no SMS dispatched. |
| Repeated `request_id` | `200 OK` with the same operation, including after completion; no second SMS. |
| Unknown check ID | `404 Not Found`. |
| Another check is active | `409 Conflict`, referencing the active operation. |
| Cooldown is active | `429 Too Many Requests` with `Retry-After`. |
| Modem cannot accept new work | `503 Service Unavailable`; stored results remain retrievable. |
| Carrier silence or unparseable reply | Operation eventually reports `timed_out` or a sanitized `failed` outcome; never fabricate a zero balance. |

Operation failure is represented by the retrieved operation's status; a successful HTTP GET does not mean the balance check succeeded. Duplicate-request lookup must precede availability and cooldown checks so clients can recover the original result.

## Existing health API

`GET /api/v1/health` is an existing authenticated endpoint described in the repository contract. It returns only JSON `true` with `200 OK`, or JSON `false` with `503 Service Unavailable`.

```powershell
Invoke-RestMethod `
    -Method Get `
    -Uri "$modemApiBase/api/v1/health" `
    -Headers $modemApiHeaders
```

Health indicates modem/service readiness under the existing health rules. It does not indicate whether a fresh balance is available. Some PowerShell versions throw on a `503` response; handle that HTTP status explicitly in monitoring clients.

## Reporting and compatibility

REST clients retrieve results by polling. Balance callbacks are not implemented. Existing `communication.sent`, `communication.delivered`, and `communication.failed` webhooks retain their limited payload and do not report balances.

The existing `POST /api/v1/communications` SMS/call contract remains separate. The desktop UI accesses balance operations through Tauri and the local service protocol; browser code must not access the modem or named pipe directly.

## Carrier and hardware acceptance

The current profile retains the existing `TK` SMS to `191` query. Only a new complete reply from `191`, within the persisted query window, with exactly one recognized main-account label and a valid monetary amount can complete a check. Supported labels are `TK goc`, `TK chinh`, `Tai khoan goc`, and `Tai khoan chinh`, including Vietnamese accented forms. Supported units are `d`, `đ`, `dong`, and `VND`. Dot/comma thousands groups must contain three digits. Bonus-only, data-package, postpaid-charge, ambiguous and malformed replies do not become a balance result.

Verify that this query returns the desired main monetary balance for the installed SIM before production use. A five-minute quarantine reduces late-reply ambiguity; carrier SMS has no request ID, so exact attribution of arbitrarily delayed replies cannot be guaranteed. The current hardware status does not expose SIM identity: after replacing the SIM, treat stored snapshots as belonging to the previous SIM until a successful check of the new SIM. Automatic SIM-swap invalidation is not implemented.

For desktop transport clients, the additive commands are `start_balance_check` with `request_id`, `get_balance_check` with `id`, and `get_latest_balance`. Tauri uses `requestId` for the start command argument. Existing `check_balance` and `list_balance_checks` command names remain supported; the synchronous legacy check now shares the durable workflow and can outlast older client timeouts. Prefer the asynchronous commands.
