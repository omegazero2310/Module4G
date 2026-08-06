import { act, render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Status } from "./types";

const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock("./api", () => ({ invoke }));

import { App } from "./App";

const ready: Status = {
  serviceVersion: "1.0.0",
  state: "Ready",
  port: "COM7",
  simState: "READY",
  registration: "registered",
  signalRssi: 20,
  lastError: "",
  deliveryTrackingAvailable: true,
  deliveryTrackingError: "",
};

describe("application shell", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    invoke.mockResolvedValue(ready);
  });

  it("keeps navigation labels and the two-second status polling cadence", async () => {
    render(<App />);
    expect(screen.getByRole("button", { name: "Dashboard" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "SMS" })).toBeInTheDocument();
    await act(async () => Promise.resolve());
    expect(invoke).toHaveBeenCalledWith("get_status");
    expect(invoke).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTimeAsync(2_000));
    expect(invoke).toHaveBeenCalledTimes(2);
  });
});
