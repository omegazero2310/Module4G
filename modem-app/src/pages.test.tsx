import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { Calls, SettingsPage, Sms } from "./pages";
import type { Record, Settings } from "./types";

const invokeMock = vi.fn();
vi.mock("./api", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

const smsRecord = (id: string, direction: string, body: string): Record => ({
  id,
  direction,
  body,
  peer: "redacted",
  state: "received",
  createdAtMs: 1,
  partCount: 1,
  partsReceived: 1,
  multipartComplete: true,
} as Record);

beforeEach(() => invokeMock.mockReset());

describe("SMS page", () => {
  it("keeps the 15-second polling cadence and filters by direction", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    invokeMock.mockResolvedValue([
      smsRecord("in", "inbound", "incoming message"),
      smsRecord("out", "outbound", "outgoing message"),
    ]);
    render(<Sms ready/>);
    expect(await screen.findByText("incoming message")).toBeInTheDocument();
    await userEvent.click(screen.getByRole("tab", { name: /Outbound 1/ }));
    expect(screen.queryByText("incoming message")).not.toBeInTheDocument();
    expect(screen.getByText("outgoing message")).toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(15_000);
    expect(invokeMock).toHaveBeenCalledWith("list_sms");
    expect(invokeMock.mock.calls.filter(([command]) => command === "list_sms")).toHaveLength(2);
    vi.useRealTimers();
  });
});

describe("Calls page", () => {
  it("rejects non-AMR uploads and keeps dialing disabled without selected audio", async () => {
    invokeMock.mockResolvedValue({ calls: [], audio: [] });
    render(<Calls ready/>);
    expect(await screen.findByText("No audio uploaded. Dialing is disabled.")).toBeInTheDocument();
    const picker = document.querySelector('input[type="file"]') as HTMLInputElement;
    fireEvent.change(picker, { target: { files: [new File(["text"], "message.txt")] } });
    expect(screen.getByText("Select an AMR-NB file with an .amr extension.")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Dial" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Hang Up" })).toBeDisabled();
  });
});

describe("Settings page", () => {
  it("serializes the complete camelCase settings object", async () => {
    const settings: Settings = {
      usbVid: 7531,
      usbPid: 1,
      portOverride: "",
      baud: 115200,
      callTimeoutSeconds: 45,
      uploadPacingMs: 5,
      maxAudioBytes: 1024,
      ussdCode: "*101#",
      ussdTimeoutSeconds: 30,
      currency: "VND",
      lowBalanceThreshold: 10000,
      balanceRegex: "balance",
    };
    const integration = {
      restEnabled: false, restBindAddress: "0.0.0.0:5069",
      webhookUrl: "http://10.1.11.117:5068/api/v1/webhooks/receive",
      hasRestToken: false, hasWebhookToken: false, restToken: "", webhookToken: "",
      clearRestToken: false, clearWebhookToken: false,
    };
    invokeMock.mockImplementation((command: string, args?: { settings: Settings }) => {
      if (command === "get_settings") return Promise.resolve(settings);
      if (command === "get_integration_settings") return Promise.resolve(integration);
      return Promise.resolve(args?.settings);
    });
    render(<SettingsPage/>);
    const override = await screen.findByLabelText("Port override");
    const restToken = screen.getByLabelText("Replace REST bearer token") as HTMLInputElement;
    const copyToken = screen.getByRole("button", { name: "Copy token" });
    expect(copyToken).toBeDisabled();
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
    await userEvent.click(screen.getByRole("button", { name: "Generate token" }));
    expect(restToken.value).toMatch(/^[0-9a-f]{64}$/);
    expect(copyToken).toBeEnabled();
    await userEvent.click(copyToken);
    expect(writeText).toHaveBeenCalledWith(restToken.value);
    expect(screen.getByText("REST bearer token copied to the clipboard.")).toBeInTheDocument();
    expect(screen.getByLabelText("Clear stored REST token")).not.toBeChecked();
    await userEvent.type(override, "COM9");
    await userEvent.click(screen.getByRole("button", { name: "Save settings" }));
    await waitFor(() => expect(invokeMock).toHaveBeenCalledWith("update_settings", {
      settings: { ...settings, portOverride: "COM9" },
    }));
  });
});
