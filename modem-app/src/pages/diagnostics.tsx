import { type FormEvent, useEffect, useState } from "react";
import { invoke } from "../api";
import type { IntegrationDiagnosticEvent, Port, Status } from "../types";
import { hex } from "../format";

export function Diagnostics({ status }: { status: Status }) {
  const [ports, setPorts] = useState<Port[]>([]);
  const [events, setEvents] = useState<IntegrationDiagnosticEvent[]>([]);
  const [error, setError] = useState("");
  const [integrationError, setIntegrationError] = useState("");
  const [command, setCommand] = useState("AT+CSQ");
  const [history, setHistory] = useState<{ at: string; command: string; lines: string[] }[]>([]);
  const [running, setRunning] = useState(false);
  const refresh = () => invoke<Port[]>("list_ports").then(setPorts).catch(e => setError(String(e)));
  const refreshIntegration = () => invoke<IntegrationDiagnosticEvent[]>("list_integration_diagnostics")
    .then(events => { setEvents(events); setIntegrationError(""); })
    .catch(e => setIntegrationError(String(e)));
  useEffect(() => { void refresh(); }, []);
  useEffect(() => {
    void refreshIntegration();
    const timer = window.setInterval(() => { void refreshIntegration(); }, 2_000);
    return () => window.clearInterval(timer);
  }, []);
  const run = async (e: FormEvent) => {
    e.preventDefault(); setRunning(true); setError("");
    try { const lines = await invoke<string[]>("execute_at", { command }); setHistory(h => [{ at: new Date().toLocaleTimeString(), command, lines }, ...h]); }
    catch (e) { setError(String(e)); } finally { setRunning(false); }
  };
  return <div className="stack">
    <div className="panel"><div className="panel-title"><strong>COM candidates</strong><button onClick={refresh}>Refresh</button></div>{ports.length ? <table><thead><tr><th>Port</th><th>VID:PID</th><th>Interface</th></tr></thead><tbody>{ports.map(p => <tr key={p.name}><td>{p.name}{p.dedicatedAt && <span className="badge">preferred</span>}</td><td>{hex(p.vid)}:{hex(p.pid)}</td><td>{p.label || "USB serial interface"}</td></tr>)}</tbody></table> : <p>No matching COM candidates detected.</p>}</div>
    <div className="panel"><div className="panel-title"><strong>Integration activity</strong><button onClick={refreshIntegration}>Refresh</button></div><p className="muted">Capture requires <code>MODEMD_INTEGRATION_DEBUG=1</code> when the daemon starts. Events are sanitized, kept only in memory, and cleared when the service restarts.</p>{integrationError ? <p className="form-error">{integrationError}</p> : events.length ? <table><thead><tr><th>Time</th><th>Source</th><th>Phase</th><th>Outcome</th><th>IDs</th><th>Duration</th><th>Summary</th></tr></thead><tbody>{events.map((event, index) => <tr key={`${event.timestamp}-${index}`}><td>{new Date(event.timestamp).toLocaleTimeString()}</td><td>{event.source}</td><td>{event.phase}</td><td>{event.outcome}{event.httpStatus !== undefined && ` (${event.httpStatus})`}</td><td>{[event.requestId, event.communicationId].filter(Boolean).join(" / ") || "—"}</td><td>{event.elapsedMs === undefined ? "—" : `${event.elapsedMs} ms`}</td><td>{event.summary}</td></tr>)}</tbody></table> : <p>No integration activity captured. Start the daemon with <code>MODEMD_INTEGRATION_DEBUG=1</code>, then send a REST communication or wait for a webhook attempt.</p>}</div>
    <form className="panel" onSubmit={run}><strong>Guarded AT console</strong><p className="muted">Interactive workflow commands are blocked.</p><div className="row"><input value={command} onChange={e => setCommand(e.target.value)} /><button disabled={running || status.state !== "Ready"}>{running ? "Running…" : "Execute"}</button></div>{error && <p className="form-error">{error}</p>}<div className="terminal">{history.length ? history.map((x, i) => <div key={i}><span>{x.at}</span> &gt; {x.command}<pre>{x.lines.join("\n")}</pre></div>) : <p>No commands in this session.</p>}</div></form>
  </div>;
}
