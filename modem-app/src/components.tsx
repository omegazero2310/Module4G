import React, { useEffect } from "react";
import { smsStateLabel } from "./format";
import type { Record } from "./types";

export function Card({ label, value }: { label: string; value: string }) {
  return <article><small>{label}</small><strong>{value || "—"}</strong></article>;
}

export function History({ title, records, empty, body = false }: { title: string; records: Record[]; empty: string; body?: boolean }) {
  const calls = title === "Call history";
  return <div className="panel"><strong>{title}</strong>{records.length ? <table><thead><tr><th>Time</th><th>Peer</th>{body && <th>Message/response</th>}<th>Progress</th>{calls && <><th>Answer</th><th>End reason</th><th>Detail</th></>}</tr></thead><tbody>{records.map(record => <tr key={record.id}><td>{new Date(record.createdAtMs).toLocaleString()}</td><td>{record.peer || "—"}</td>{body && <td>{record.body || record.detail || "—"}</td>}<td><span className="badge">{record.state}</span></td>{calls && <><td>{record.answerClassification || "unknown"}</td><td>{record.endReason || "—"}</td><td>{record.error || record.releaseCause || "—"}</td></>}</tr>)}</tbody></table> : <p className="muted">{empty}</p>}</div>;
}

export function DetailDialog({ record, onClose }: { record: Record; onClose: () => void }) {
  useEffect(() => {
    const key = (event: KeyboardEvent) => { if (event.key === "Escape") onClose(); };
    window.addEventListener("keydown", key);
    return () => window.removeEventListener("keydown", key);
  }, [onClose]);
  const fields = [["Direction", record.direction], ["Peer", record.peer], ["Status", smsStateLabel(record.state)], ["Kind", record.kind], ["Source", record.source], ["SIM locations", record.storage && `${record.storage} / ${(record.storageIndices?.length ? record.storageIndices : [record.storageIndex]).join(", ")}`], ["Parts", record.partCount > 1 ? `${record.partsReceived}/${record.partCount}${record.multipartComplete ? " complete" : " incomplete"}` : ""], ["Modem status", record.modemStatus], ["Modem timestamp", record.modemTimestamp], ["Encoding / DCS", record.encoding && `${record.encoding} / ${record.dcs}`], ["Length", String(record.length || "")], ["Service center", record.serviceCenter], ["Report requested", record.deliveryReportRequested ? "Yes" : "No"], ["Message reference", record.messageReference], ["TP status", record.deliveryStatus], ["Service-centre timestamp", record.deliveryReportScts], ["Discharge time", record.deliveryReportDischargeTime], ["Cause", record.cause], ["Tracking degradation", record.deliveryTrackingError], ["On modem", record.source === "sim" ? (record.presentOnModem ? "Yes" : "No") : ""]].filter(field => field[1]);
  return <div className="modal-backdrop" role="presentation" onMouseDown={event => { if (event.target === event.currentTarget) onClose(); }}><div className="modal" role="dialog" aria-modal="true" aria-labelledby="message-title"><div className="panel-title"><strong id="message-title">Message detail</strong><button autoFocus onClick={onClose}>Close</button></div><pre className="full-message">{record.body || record.detail || "(empty message)"}</pre><dl>{fields.map(([label, value]) => <React.Fragment key={label}><dt>{label}</dt><dd>{value}</dd></React.Fragment>)}</dl></div></div>;
}
