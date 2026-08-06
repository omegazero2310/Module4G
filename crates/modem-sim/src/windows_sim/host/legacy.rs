use super::*;

pub(super) fn response(command: &str, scenario: &mut CallScenario) -> &'static str {
    let command = command.trim();
    if command.contains("\"command\":\"sync_sms\"") {
        return "{\"ok\":true,\"data\":{\"count\":7}}\n";
    }
    if command.contains("\"command\":\"list_sms\"") {
        return "{\"ok\":true,\"data\":[{\"id\":\"sim-unread\",\"direction\":\"inbound\",\"peer\":\"191\",\"body\":\"Balance line one\\nBalance line two\",\"state\":\"unread\",\"detail\":\"\",\"createdAtMs\":1785722400000,\"answerClassification\":\"\",\"endReason\":\"\",\"alertingAtMs\":0,\"releaseCause\":\"\",\"kind\":\"received\",\"source\":\"sim\",\"storage\":\"SM\",\"storageIndex\":1,\"modemStatus\":\"REC UNREAD\",\"modemTimestamp\":\"26/08/03,10:00:00+28\",\"encoding\":\"GSM\",\"dcs\":0,\"length\":33,\"serviceCenter\":\"\",\"messageReference\":\"\",\"deliveryStatus\":\"\",\"synchronizedAtMs\":1785722400000,\"presentOnModem\":true,\"smsId\":\"\"}]}\n";
    }
    if command.contains("\"command\":\"list_balances\"") {
        return "{\"ok\":true,\"data\":[]}\n";
    }
    if command.contains("\"command\":\"check_balance\"") {
        return "{\"ok\":true,\"data\":{\"id\":\"balance-1\",\"raw\":\"Tai khoan goc: 85.500d\\nKhuyen mai: 89.174d\",\"value\":null,\"currency\":\"\",\"error\":\"\",\"createdAtMs\":1785722400000,\"smsId\":\"balance-sms-1\"}}\n";
    }
    if command.contains("\"command\":\"send_sms\"") {
        return "{\"ok\":true,\"data\":{\"id\":\"sent-1\",\"direction\":\"outbound\",\"peer\":\"+84912345678\",\"body\":\"simulated\",\"state\":\"submitted\",\"detail\":\"\",\"createdAtMs\":1785722400000,\"answerClassification\":\"\",\"endReason\":\"\",\"alertingAtMs\":0,\"releaseCause\":\"\",\"kind\":\"submitted\",\"source\":\"app\",\"storage\":\"\",\"storageIndex\":-1,\"modemStatus\":\"\",\"modemTimestamp\":\"\",\"encoding\":\"GSM-7\",\"dcs\":-1,\"length\":9,\"serviceCenter\":\"\",\"messageReference\":\"42\",\"deliveryStatus\":\"\",\"synchronizedAtMs\":0,\"presentOnModem\":false,\"smsId\":\"\"}}\n";
    }
    if command.starts_with("SMS|") {
        return "+CMGS: 42\r\nOK\n";
    }
    if command == "BALANCE" {
        return "Thuê bao: 84XXXXXXXXX (HISCL):\r\n- TK gốc: 85.500đ, HSD: 00:00:00 02-10-2026.\r\n- TK tiền di động: 863đ, HSD: 00:00:00 01-01-2100.\r\n- TK tiền khuyến mại: 89.174đ.\r\nĐể tìm hiểu và đăng ký các gói data ưu đãi, truy cập https://vietteltelecom.vn/goidatahot\n";
    }
    if command.starts_with("USSD|") {
        return "+CUSD: 0,\"Balance 125.50 THB\",15\r\nOK\n";
    }
    if let Some(number) = command.strip_prefix("DIAL|") {
        *scenario = match &number[number.len().saturating_sub(2)..] {
            "02" => CallScenario::NoAnswer,
            "03" => CallScenario::Busy,
            "04" => CallScenario::Unreachable,
            "05" => CallScenario::NetworkError,
            "06" => CallScenario::MissingTerminal,
            _ => CallScenario::Answered,
        };
        return "OK\n";
    }
    if command == "HANGUP" {
        return "OK\n";
    }
    if command == "CALLCAUSE" {
        return match scenario {
            CallScenario::Unreachable => "+CEER: 20 Subscriber absent\n",
            CallScenario::NetworkError => "+CEER: 34 No circuit/channel available\n",
            _ => "+CEER: 16 Normal call clearing\n",
        };
    }
    if command == "CALLSTATUS" {
        return match scenario {
            CallScenario::Answered => {
                "+CLCC: 1,0,0,0,0 | VOICE CALL: BEGIN | VOICE CALL: END | NO CARRIER\n"
            }
            CallScenario::NoAnswer => "NO ANSWER | VOICE CALL: END | NO CARRIER\n",
            CallScenario::Busy => "BUSY | NO CARRIER\n",
            CallScenario::Unreachable | CallScenario::NetworkError => "NO CARRIER\n",
            CallScenario::MissingTerminal => "+CLCC: 1,0,2,0,0\n",
        };
    }
    match command {
        "STATUS" => "STATUS\t0.1.0-sim\tReady\tSIMULATED\tREADY\tRegistered\t20\n",
        "AT" | "AT+CMEE=2" | "AT+CVHU=0" | "AT+CLCC=1" | "AT+CMGF=1" | "AT+CNMI=2,1,0,1,0"
        | "AT+CSMP=49,167,0,0" => "OK\n",
        "AT+CNMI?" => "+CNMI: 2,1,0,1,0\r\nOK\n",
        "AT+CSMP?" => "+CSMP: 49,167,0,0\r\nOK\n",
        "ATI" => "SIMCOM_Ltd\r\nSIMCOM_SIM7600G-H\r\nRevision: A7670M7_V1.11\r\nOK\n",
        "AT+CSQ" => "+CSQ: 20,99\r\nOK\n",
        "AT+CPIN?" => "+CPIN: READY\r\nOK\n",
        "AT+CREG?" => "+CREG: 0,1\r\nOK\n",
        _ => "ERROR\n",
    }
}
