import { describe, expect, it } from "vitest";

import { accessProblem, callProblem } from "./access";
import { ApiError } from "./client";
import { signInProblem } from "./signIn";

const management = (status: number, error: string | null) =>
  new ApiError(status, error, null, error === null ? null : { error });
const dashboard = (status: number, error: string, message: string) =>
  new ApiError(status, error, message, { error, message });

describe("accessProblem", () => {
  it("reads the management API's refusals", () => {
    expect(accessProblem(management(401, "missing management key"))).toEqual({
      kind: "missing-key",
    });
    expect(accessProblem(management(401, "invalid management key"))).toEqual({ kind: "wrong-key" });
    expect(accessProblem(management(403, "remote management disabled"))).toEqual({
      kind: "remote-disabled",
    });
    expect(
      accessProblem(
        management(403, "IP banned due to too many failed attempts. Try again in 29m41s"),
      ),
    ).toEqual({ kind: "banned", retryIn: "29m41s" });
    expect(accessProblem(management(403, "remote management key not set"))).toEqual({
      kind: "management-off",
    });
  });

  it("reads the dashboard API's refusals", () => {
    expect(accessProblem(dashboard(401, "missing_management_key", "x"))).toEqual({
      kind: "missing-key",
    });
    expect(accessProblem(dashboard(401, "invalid_management_key", "x"))).toEqual({
      kind: "wrong-key",
    });
    expect(accessProblem(dashboard(403, "remote_management_disabled", "x"))).toEqual({
      kind: "remote-disabled",
    });
    expect(
      accessProblem(
        dashboard(403, "ip_banned", "IP banned due to too many failed attempts. Try again in 12m3s."),
      ),
    ).toEqual({ kind: "banned", retryIn: "12m3s" });
    expect(accessProblem(dashboard(403, "ip_banned", "banned"))).toEqual({
      kind: "banned",
      retryIn: null,
    });
    expect(accessProblem(dashboard(404, "management_disabled", "x"))).toEqual({
      kind: "management-off",
    });
  });

  it("leaves other answers alone", () => {
    expect(accessProblem(management(404, null))).toBeNull();
    expect(accessProblem(dashboard(400, "invalid_request", "x"))).toBeNull();
    expect(accessProblem(new Error("x"))).toBeNull();
    expect(accessProblem(new ApiError(0, null, null, null))).toEqual({ kind: "unreachable" });
  });
});

describe("callProblem", () => {
  it("sorts what isn't about access", () => {
    expect(callProblem(dashboard(503, "ledger_unavailable", "x"))).toEqual({
      kind: "ledger-unavailable",
    });
    expect(callProblem(management(404, null))).toEqual({ kind: "unsupported" });
    expect(callProblem(dashboard(404, "not_found", "no such log"))).toEqual({ kind: "not-found" });
    expect(callProblem(dashboard(400, "invalid_cursor", "bad cursor"))).toEqual({
      kind: "invalid",
      message: "bad cursor",
    });
    expect(callProblem(dashboard(500, "internal_error", "disk full"))).toEqual({
      kind: "server-error",
      status: 500,
      message: "disk full",
    });
    expect(callProblem(management(418, "teapot"))).toEqual({
      kind: "unexpected",
      status: 418,
      message: "teapot",
    });
  });
});

describe("signInProblem", () => {
  it("reads the check's empty 404 as management being off", () => {
    expect(signInProblem(management(404, null))).toEqual({ kind: "management-off" });
  });
});
