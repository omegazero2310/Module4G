import { useEffect, useState } from "react";
import { invoke } from "./api";
import { Balance, Calls, Dashboard, Diagnostics, SettingsPage, Sms } from "./pages";
import type { Status } from "./types";
import "./style.css";

const empty: Status = { serviceVersion: "—", state: "Connecting", port: "", simState: "—", registration: "—", signalRssi: 0, lastError: "", deliveryTrackingAvailable: false, deliveryTrackingError: "" };
const tabs = ["Dashboard", "SMS", "Calls", "Balance", "Diagnostics", "Settings"] as const;

export function App() {
  const [tab, setTab] = useState<(typeof tabs)[number]>("Dashboard");
  const [status, setStatus] = useState(empty);
  useEffect(() => {
    let active = true;
    const refresh = () => invoke<Status>("get_status").then((next) => active && setStatus(next)).catch((error) => active && setStatus((current) => ({ ...current, state: "Service unavailable", lastError: String(error) })));
    refresh();
    const timer = setInterval(refresh, 2000);
    return () => { active = false; clearInterval(timer); };
  }, []);
  const ready = status.state === "Ready";
  return <div className="shell"><aside><h1>A7670</h1><p>Modem Console</p><nav>{tabs.map((item) => <button key={item} className={tab === item ? "active" : ""} onClick={() => setTab(item)}>{item}</button>)}</nav></aside><main><header><div><span className={`dot ${ready ? "ready" : ""}`}/>{status.state}</div><small>{status.port || "No port"}</small></header><section><h2>{tab}</h2>{tab === "Dashboard" ? <Dashboard status={status}/> : tab === "SMS" ? <Sms ready={ready}/> : tab === "Calls" ? <Calls ready={ready}/> : tab === "Balance" ? <Balance ready={ready}/> : tab === "Diagnostics" ? <Diagnostics status={status}/> : <SettingsPage/>}</section></main></div>;
}
