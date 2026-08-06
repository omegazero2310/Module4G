import { beforeEach, describe, expect, it, vi } from "vitest";

const { tauriInvoke } = vi.hoisted(() => ({ tauriInvoke: vi.fn() }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: tauriInvoke }));

import { invoke } from "./api";

describe("Tauri API boundary", () => {
  beforeEach(() => tauriInvoke.mockResolvedValue({ ok: true }));

  it("preserves command names and camelCase payloads", async () => {
    const payload = { destination: "191", body: "TK", audioId: "audio-1" };
    await invoke("send_sms", payload);
    expect(tauriInvoke).toHaveBeenCalledWith("send_sms", payload);
  });

  it("does not synthesize arguments for polling commands", async () => {
    await invoke("get_status");
    expect(tauriInvoke).toHaveBeenCalledWith("get_status", undefined);
  });
});
