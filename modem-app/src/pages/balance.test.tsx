import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { Balance } from "./balance";

const invokeMock = vi.fn();
vi.mock("../api", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
const latest = { balance: null, freshness: "unavailable", active_check_id: null, retry_after_seconds: 0 };
const operation = { id: "check-1", request_id: "key", status: "waiting_reply", balance: null, failure_reason: null };
beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation((command: string) => Promise.resolve(command === "get_latest_balance" ? latest : []));
});
afterEach(() => { cleanup(); vi.useRealTimers(); });

describe("Balance page", () => {
  it("distinguishes unavailable from a confirmed zero balance", async () => {
    const view = render(<Balance ready/>);
    expect(await screen.findByText("Balance unavailable")).toBeInTheDocument();
    view.unmount();
    invokeMock.mockImplementation((command: string) => Promise.resolve(command === "get_latest_balance" ? { ...latest, balance: { amount_vnd: 0, observed_at: "2026-10-08T08:00:00Z" }, freshness: "fresh" } : []));
    render(<Balance ready/>);
    expect(await screen.findByText("0 VND")).toBeInTheDocument();
    expect(screen.getByText(/Last checked:/)).toBeInTheDocument();
  });
  it("starts asynchronously and disables duplicate submission", async () => {
    invokeMock.mockImplementation((command: string) => Promise.resolve(command === "get_latest_balance" ? latest : ["start_balance_check", "get_balance_check"].includes(command) ? operation : []));
    render(<Balance ready/>);
    await screen.findByText("Balance unavailable");
    fireEvent.click(screen.getByRole("button", { name: "Check balance" }));
    expect(await screen.findByText("Waiting for Viettel reply…")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check balance" })).toBeDisabled();
    expect(invokeMock).toHaveBeenCalledWith("start_balance_check", { requestId: expect.any(String) });
  });
  it("resumes an active operation after reload and cleans up polling", async () => {
    vi.useFakeTimers({ shouldAdvanceTime: true });
    invokeMock.mockImplementation((command: string) => Promise.resolve(command === "get_latest_balance" ? { ...latest, active_check_id: "check-1" } : command === "get_balance_check" ? operation : []));
    const view = render(<Balance ready/>);
    expect(await screen.findByText("Waiting for Viettel reply…")).toBeInTheDocument();
    expect(invokeMock).toHaveBeenCalledWith("get_balance_check", { id: "check-1" });
    view.unmount();
    const count = invokeMock.mock.calls.length;
    await vi.advanceTimersByTimeAsync(20_000);
    expect(invokeMock.mock.calls.length).toBe(count);
  });
  it("retains a stale snapshot when refresh fails and shows a safe error", async () => {
    invokeMock.mockImplementation((command: string) => command === "start_balance_check" ? Promise.reject("Service unavailable") : Promise.resolve(command === "get_latest_balance" ? { ...latest, balance: { amount_vnd: 85500, observed_at: "2026-10-08T08:00:00Z" }, freshness: "stale" } : []));
    render(<Balance ready/>);
    expect(await screen.findByText("85,500 VND")).toBeInTheDocument();
    expect(screen.getByText("Stale snapshot")).toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Check balance" }));
    await waitFor(() => expect(screen.getByRole("alert")).toHaveTextContent("Service unavailable"));
    expect(screen.getByText("85,500 VND")).toBeInTheDocument();
  });
  it("shows cooldown and disables checks when hardware is unavailable", async () => {
    invokeMock.mockImplementation((command: string) => Promise.resolve(command === "get_latest_balance" ? { ...latest, retry_after_seconds: 60 } : []));
    const view = render(<Balance ready={false}/>);
    expect(await screen.findByText(/Wait 60 seconds/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Check balance" })).toBeDisabled();
    view.unmount();
  });
  it("reuses a lost-response key and clears it after a terminal replay", async () => {
    let starts = 0;
    invokeMock.mockImplementation((command: string) => {
      if (command === "start_balance_check") {
        starts++;
        return starts === 1 ? Promise.reject("Connection lost") : Promise.resolve({ ...operation, status: "succeeded" });
      }
      return Promise.resolve(command === "get_latest_balance" ? latest : []);
    });
    render(<Balance ready/>);
    await screen.findByText("Balance unavailable");
    const button = screen.getByRole("button", { name: "Check balance" });
    fireEvent.click(button);
    await screen.findByText("Connection lost");
    fireEvent.click(button);
    await screen.findByText("Balance updated.");
    await waitFor(() => expect(button).toBeEnabled());
    fireEvent.click(button);
    await waitFor(() => expect(starts).toBe(3));
    const keys = invokeMock.mock.calls.filter(([command]) => command === "start_balance_check").map(([, args]) => args.requestId);
    expect(keys[0]).toBe(keys[1]);
    expect(keys[2]).not.toBe(keys[1]);
  });
});
