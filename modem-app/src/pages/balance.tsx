import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "../api";
import type { BalanceCheck, LatestBalance, Record } from "../types";
import { DetailDialog } from "../components";

const terminal = (check: BalanceCheck) => ["succeeded", "failed", "timed_out"].includes(check.status);
const messages: { [K in BalanceCheck["status"]]: string } = {
  queued: "Queued for modem…", sending: "Sending balance query…",
  waiting_reply: "Waiting for Viettel reply…",
  send_unknown: "SMS submission is uncertain. Waiting for a confirmed reply…",
  succeeded: "Balance updated.", failed: "Balance check failed.", timed_out: "Balance check timed out.",
};

export function Balance({ ready }: { ready: boolean }) {
  const [snapshot, setSnapshot] = useState<LatestBalance | null>(null);
  const [records, setRecords] = useState<Record[]>([]);
  const [operation, setOperation] = useState<BalanceCheck | null>(null);
  const [starting, setStarting] = useState(false);
  const [error, setError] = useState("");
  const [selected, setSelected] = useState<Record | null>(null);
  const [pollVersion, setPollVersion] = useState(0);
  const activeId = useRef<string | null>(null);
  const requestId = useRef<string | null>(null);
  const pollingStarted = useRef(0);
  const mounted = useRef(false);
  const refresh = useCallback(() => setPollVersion(version => version + 1), []);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let backoff = 2000;
    const poll = async () => {
      let delay = 15_000;
      try {
        const latest = await invoke<LatestBalance>("get_latest_balance");
        if (cancelled) return;
        setSnapshot(latest);
        const id = latest.active_check_id ?? activeId.current;
        if (id) {
          if (!pollingStarted.current) pollingStarted.current = Date.now();
          const check = await invoke<BalanceCheck>("get_balance_check", { id });
          if (cancelled) return;
          setOperation(check);
          activeId.current = terminal(check) ? null : id;
          if (terminal(check)) {
            requestId.current = null; pollingStarted.current = 0;
            const [updated, history] = await Promise.all([
              invoke<LatestBalance>("get_latest_balance"), invoke<Record[]>("list_balance_checks"),
            ]);
            if (cancelled) return;
            setSnapshot(updated); setRecords(history);
          } else {
            delay = 2000;
            if (Date.now() - pollingStarted.current >= 180_000) {
              setError("Stopped waiting. The service keeps the check; refresh to resume.");
              return;
            }
          }
        } else {
          const history = await invoke<Record[]>("list_balance_checks");
          if (cancelled) return;
          setRecords(history);
        }
        if (latest.retry_after_seconds > 0) delay = 2000;
        backoff = 2000;
      } catch (failure) {
        if (cancelled) return;
        setError(String(failure)); delay = backoff;
        backoff = Math.min(backoff * 2, 30_000);
      }
      if (!cancelled) timer = setTimeout(() => { void poll(); }, delay);
    };
    void poll();
    return () => { cancelled = true; if (timer) clearTimeout(timer); };
  }, [pollVersion]);
  const check = async () => {
    setStarting(true); setError("");
    requestId.current ??= crypto.randomUUID();
    try {
      const started = await invoke<BalanceCheck>("start_balance_check", { requestId: requestId.current });
      if (!mounted.current) return;
      setOperation(started); activeId.current = terminal(started) ? null : started.id;
      if (terminal(started)) requestId.current = null;
      pollingStarted.current = Date.now(); refresh();
    } catch (failure) {
      if (mounted.current) setError(String(failure));
      // Keep the key: retrying a lost response must not create another SMS.
    } finally { if (mounted.current) setStarting(false); }
  };
  const busy = starting || Boolean(operation && !terminal(operation)) || Boolean(snapshot?.active_check_id);
  const balance = snapshot?.balance;
  return <div className="stack">
    <section className="panel" aria-label="Viettel balance">
      <div className="panel-title balance-heading">
        <div><strong>Viettel balance</strong><p className="muted">Checks this modem's SIM by sending TK to 191.</p></div>
        <div className="balance-actions">
          <button onClick={() => { pollingStarted.current = 0; refresh(); }}>Refresh status</button>
          <button disabled={!ready || busy || Boolean(snapshot?.retry_after_seconds)} onClick={() => { void check(); }}>Check balance</button>
        </div>
      </div>
      {snapshot ? balance ? <>
        <h2>{new Intl.NumberFormat("en-US").format(balance.amount_vnd)} VND</h2>
        <p className="muted">Last checked: {new Date(balance.observed_at).toLocaleString()}</p>
        <span className="badge">{snapshot.freshness === "stale" ? "Stale snapshot" : "Recent snapshot"}</span>
      </> : <p>Balance unavailable</p> : <p className="muted">Loading balance…</p>}
      {operation && <p role="status">{messages[operation.status]}{operation.failure_reason && ` ${operation.failure_reason}`}</p>}
      {!ready && <p className="muted">Connect the modem before starting a check.</p>}
      {Boolean(snapshot?.retry_after_seconds) && <p className="muted">Wait {snapshot?.retry_after_seconds} seconds before another balance check.</p>}
      {error && <p className="form-error" role="alert">{error}</p>}
    </section>
    <section className="panel" aria-label="Balance history"><strong>Balance history</strong>
      {records.length ? <table><thead><tr><th>Observed</th><th>Carrier reply</th><th>Source</th></tr></thead><tbody>{records.map(record =>
        <tr key={record.id} className="clickable" tabIndex={0} onClick={() => setSelected(record)} onKeyDown={event => {
          if (event.key === "Enter" || event.key === " ") { event.preventDefault(); setSelected(record); }
        }}><td>{new Date(record.createdAtMs).toLocaleString()}</td><td className="preview">{record.body}</td><td>{record.smsId && <span className="badge">Linked SMS</span>}</td></tr>
      )}</tbody></table> : <p className="muted">No balance checks yet.</p>}
    </section>
    {selected && <DetailDialog record={selected} onClose={() => setSelected(null)}/>}
  </div>;
}

