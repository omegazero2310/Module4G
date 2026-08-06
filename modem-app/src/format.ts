export function hex(value:number){return value.toString(16).toUpperCase().padStart(4,"0")}
export function formatBytes(value:number){return value<1024?`${value} B`:`${(value/1024).toFixed(1)} KiB`}
export function smsStateLabel(state:string){return({sending:"Sending",submitted:"Submitted to SMSC — Delivery unconfirmed","delivery-pending":"Submitted to SMSC — delivery pending",delivered:"Delivered","delivery-failed":"Delivery failed","send-failed":"Not sent","send-unknown":"Send result unknown","delivery-unknown":"Delivery unknown"}[state]||state)}
