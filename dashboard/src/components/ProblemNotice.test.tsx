import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";

import { callProblem } from "../api/access";
import { ApiError } from "../api/client";
import { ConfigConflict } from "../api/configEditor";
import { ProblemNotice } from "./ProblemNotice";

it("names a server refusal and reloads only the owning form", async () => {
  const reload = vi.fn();
  render(<ProblemNotice problem={callProblem(new ApiError(412, "precondition_failed", null, null))} onReloadConfig={reload} live />);
  expect(screen.getByRole("status")).toHaveTextContent("The server refused this save.");
  expect(screen.getByRole("status")).toHaveTextContent("Unsaved edits will be lost on reload");
  await userEvent.click(screen.getByRole("button", { name: "Reload latest config" }));
  expect(reload).toHaveBeenCalledTimes(1);
});

it("names a local refusal before any save was sent and lists changed fields", () => {
  render(<ProblemNotice problem={callProblem(new ConfigConflict(["Retries", "Client API keys"]))} live />);
  expect(screen.getByRole("status")).toHaveTextContent("The dashboard stopped this save before sending it.");
  expect(screen.getByRole("status")).toHaveTextContent("Changed fields: Retries, Client API keys.");
  expect(screen.getByRole("status")).not.toHaveTextContent("The server refused");
});
