import { describe, expect, it } from "vitest";
import { formatBytes, hex, smsStateLabel } from "./format";

describe("wire presentation", () => {
  it("preserves SMS state labels", () => {
    expect(smsStateLabel("submitted")).toBe("Submitted to SMSC — Delivery unconfirmed");
    expect(smsStateLabel("delivery-pending")).toBe("Submitted to SMSC — delivery pending");
    expect(smsStateLabel("send-unknown")).toBe("Send result unknown");
    expect(smsStateLabel("future-state")).toBe("future-state");
  });

  it("formats byte counts and USB identifiers", () => {
    expect(formatBytes(512)).toBe("512 B");
    expect(formatBytes(1536)).toBe("1.5 KiB");
    expect(hex(0x1e0e)).toBe("1E0E");
  });
});
