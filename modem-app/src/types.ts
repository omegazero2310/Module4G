export type Status={serviceVersion:string;state:string;port:string;simState:string;registration:string;signalRssi:number;lastError:string;deliveryTrackingAvailable:boolean;deliveryTrackingError:string};
export type Port={name:string;vid:number;pid:number;label:string;available:boolean;dedicatedAt:boolean};
export type Settings={usbVid:number;usbPid:number;portOverride:string;baud:number;callTimeoutSeconds:number;uploadPacingMs:number;maxAudioBytes:number;ussdCode:string;ussdTimeoutSeconds:number;currency:string;lowBalanceThreshold:number;balanceRegex:string};
export type IntegrationSettings={restEnabled:boolean;restBindAddress:string;webhookUrl:string;hasRestToken:boolean;hasWebhookToken:boolean;restToken:string;webhookToken:string;clearRestToken:boolean;clearWebhookToken:boolean};
export type IntegrationDiagnosticEvent={timestamp:string;source:string;phase:string;outcome:string;httpStatus?:number;requestId?:string;communicationId?:string;channel?:string;byteCount?:number;payloadSha256?:string;elapsedMs?:number;summary:string};
export type Record={id:string;peer:string;body:string;state:string;detail:string;cause:string;createdAtMs:number;answerClassification:string;endReason:string;releaseCause:string;direction:string;kind:string;source:string;storage:string;storageIndex:number;storageIndices:number[];partCount:number;partsReceived:number;multipartComplete:boolean;modemStatus:string;modemTimestamp:string;encoding:string;dcs:number;length:number;serviceCenter:string;messageReference:string;deliveryStatus:string;deliveryReportRequested:boolean;deliveryReportScts:string;deliveryReportDischargeTime:string;deliveryTrackingError:string;synchronizedAtMs:number;presentOnModem:boolean;smsId:string;audioId:string;error:string;durationSeconds:number;connectedAtMs:number;endedAtMs:number};
export type UploadedAudio={id:string;name:string;format:string;size:number;modulePath:string;durationMs:number;createdAtMs:number;state:string;isCurrent:boolean};
export type AudioSyncState="pending"|"syncing"|"ready"|"deferred";
export type BalanceObservation = { amount_vnd: number; observed_at: string };
export type BalanceCheck = {
  id: string; request_id: string;
  status: "queued" | "sending" | "waiting_reply" | "send_unknown" | "succeeded" | "failed" | "timed_out";
  created_at: string; updated_at: string; completed_at: string | null;
  balance: BalanceObservation | null; failure_reason: string | null;
};
export type LatestBalance = {
  balance: BalanceObservation | null; freshness: "fresh" | "stale" | "unavailable";
  active_check_id: string | null; retry_after_seconds: number;
};
export type CallData={calls:Record[];audio:UploadedAudio[];audioSyncState:AudioSyncState};
