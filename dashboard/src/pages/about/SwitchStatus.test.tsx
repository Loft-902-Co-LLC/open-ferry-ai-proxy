import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";

import { supervisorStatus, updateStatus } from "../../test/fixtures";
import { SwitchStatus } from "./SwitchStatus";

it.each([true, false])("offers observation only for an unknown pending request (pending=%s)", (pending) => {
  render(<SwitchStatus status={updateStatus({ supervisor: supervisorStatus(), switch_request: {
    request_id: "request-1", action: "install", state: "unknown", pending, reason: "the outcome is unknown", id: null,
  } })} onRetry={vi.fn()} />);
  expect(screen.queryAllByRole("button", { name: "Read status" })).toHaveLength(pending ? 1 : 0);
});

it("offers one retry when both a request and the supervisor need observation", () => {
  render(<SwitchStatus status={updateStatus({ supervisor: supervisorStatus({ stale: true, error: "the supervisor cannot be reached" }), switch_request: {
    request_id: "request-1", action: "install", state: "unknown", pending: true, reason: "the outcome is unknown", id: null,
  } })} onRetry={vi.fn()} />);
  expect(screen.getAllByRole("button")).toHaveLength(1);
  expect(screen.getByRole("button", { name: "Try again" })).toBeVisible();
});

it.each(["preparing", "accepted"] as const)("hides cached %s while the parent shows a transport error", (state) => {
  render(<SwitchStatus hidePhase status={updateStatus({ supervisor: supervisorStatus({ stale: true, error: "an older read failed" }), switch_request: {
    request_id: "request-1", action: "install", state, pending: true, reason: null, id: null,
  } })} onRetry={vi.fn()} />);
  expect(screen.queryByText("Preparing the switch")).not.toBeInTheDocument();
  expect(screen.queryByText("Switch accepted")).not.toBeInTheDocument();
  expect(screen.queryByRole("button")).not.toBeInTheDocument();
});


it.each(["switching", "stopping"])("titles supervisor state %s without calling it a switch phase", (phase) => {
  render(<SwitchStatus status={updateStatus({ supervisor: supervisorStatus({ phase, switch: null }) })} onRetry={vi.fn()} />);
  expect(screen.getByText(`The supervisor is ${phase}`)).toBeVisible();
  expect(screen.queryByText(`Switch: ${phase}`)).not.toBeInTheDocument();
});


it.each([true, false])("offers a recovery read only when polling has stopped (polling=%s)", (polling) => {
  const retry = vi.fn();
  render(<SwitchStatus recoveringSupervisor polling={polling} status={updateStatus({ supervisor: supervisorStatus({ phase: "degraded" }) })} onRetry={retry} />);
  expect(screen.getByText("The supervisor is recovering")).toBeVisible();
  expect(screen.queryByText("The supervisor is restarting")).not.toBeInTheDocument();
  if (polling) expect(screen.queryByRole("button")).not.toBeInTheDocument();
  else {
    fireEvent.click(screen.getByRole("button", { name: "Read status" }));
    expect(retry).toHaveBeenCalledTimes(1);
  }
});
