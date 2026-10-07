import { describe, expect, it } from "vitest";

import type { CooldownReason } from "../../api/credentials";
import { cooldown, credential } from "../../test/fixtures";
import {
  canReset,
  credentialCooldowns,
  credentialHealth,
  explainCooldown,
  explainReason,
  explainSignInError,
  explainStartError,
  modelCooldowns,
  pastedAddressProblem,
  providerName,
  reasonOfMessage,
  timeLeft,
} from "./credentialStates";

const REASONS: CooldownReason[] = [
  "credential_quota",
  "quota",
  "cloudflare_challenge",
  "model_not_supported",
  "invalid_grant",
  "unauthorized",
  "payment_required",
  "not_found",
  "transient_error",
  "unknown",
];

describe("the cooldown reasons", () => {
  it("explain every code the server sends, each differently, with what to do", () => {
    const titles = new Set<string>();
    for (const reason of REASONS) {
      const text = explainReason(reason);
      expect(text.title).not.toBe("");
      expect(text.meaning).not.toBe("");
      expect(text.action).not.toBe("");
      titles.add(text.title);
    }
    expect(titles.size).toBe(REASONS.length);
  });

  it("read a code they don't know as an unnamed failure", () => {
    expect(explainReason("some_new_reason")).toEqual(explainReason("unknown"));
    expect(explainReason("toString")).toEqual(explainReason("unknown"));
  });

  it("are found in the status messages the server leaves on a failed credential", () => {
    expect(reasonOfMessage("quota exhausted")).toBe("quota");
    expect(reasonOfMessage("cloudflare challenge")).toBe("cloudflare_challenge");
    expect(reasonOfMessage("invalid_grant")).toBe("invalid_grant");
    expect(reasonOfMessage("unauthorized")).toBe("unauthorized");
    expect(reasonOfMessage("payment_required")).toBe("payment_required");
    expect(reasonOfMessage("not_found")).toBe("not_found");
    expect(reasonOfMessage("model_not_supported")).toBe("model_not_supported");
    expect(reasonOfMessage(" transient upstream error ")).toBe("transient_error");
    expect(reasonOfMessage("removed via management api")).toBeNull();
    expect(reasonOfMessage("upstream said no")).toBeNull();
  });
});

describe("a credential's health", () => {
  it("is off when turned off, whatever else is true", () => {
    const health = credentialHealth(
      credential({ disabled: true, status: "disabled", cooldowns: [cooldown("quota", 60)] }),
    );
    expect(health).toMatchObject({ tone: "neutral", label: "Off" });
    expect(health.action).toBe("Turn it on to use it again.");
  });

  it("is resting for its soonest credential cooldown, with that reason's advice", () => {
    const health = credentialHealth(
      credential({
        cooldowns: [
          cooldown("credential_quota", 3600),
          cooldown("cloudflare_challenge", 90),
          cooldown("quota", 30, { scope: "model", model_key: "claude-opus-4-1" }),
        ],
      }),
    );
    expect(health).toMatchObject({
      tone: "warn",
      label: "Resting",
      summary: explainReason("cloudflare_challenge").title,
      action: explainReason("cloudflare_challenge").action,
    });
  });

  it("is failing with the reason a known status message names", () => {
    const health = credentialHealth(credential({ status: "error", status_message: "invalid_grant" }));
    expect(health).toMatchObject({
      tone: "danger",
      label: "Failing",
      summary: "The sign-in has expired or was revoked",
    });
    expect(health.action).toMatch(/^Sign in again/);
  });

  it("is failing with the server's own words for any other message", () => {
    expect(credentialHealth(credential({ status: "error", status_message: "boom" })).summary).toBe(
      "Its last request failed: boom",
    );
    expect(credentialHealth(credential({ status: "error", status_message: "" })).summary).toBe(
      "Its last request failed.",
    );
  });

  it("says when it is refreshing, waiting, resting without a cooldown, or ready", () => {
    expect(credentialHealth(credential({ status: "refreshing" })).label).toBe("Refreshing");
    expect(credentialHealth(credential({ status: "pending" })).label).toBe("Waiting");
    expect(credentialHealth(credential({ unavailable: true }))).toMatchObject({
      tone: "warn",
      label: "Resting",
    });
    expect(credentialHealth(credential())).toEqual({
      tone: "ok",
      label: "Ready",
      summary: "In use.",
      action: null,
    });
    expect(
      credentialHealth(
        credential({ cooldowns: [cooldown("quota", 30, { scope: "model", model_key: "m" })] }),
      ).summary,
    ).toBe("In use, with some models resting: see below.");
    expect(credentialHealth(credential({ status: "unknown" })).label).toBe("Unknown");
  });

  it("names the window used up when it rests for quota and the provider said which", () => {
    const weekly = {
      observed_at: "2026-10-05T11:58:00.000Z",
      signals: {
        "Anthropic-Ratelimit-Unified-5h-Utilization": "0.2",
        "Anthropic-Ratelimit-Unified-7d-Status": "rejected",
        "Anthropic-Ratelimit-Unified-Representative-Claim": "seven_day",
      },
    };
    const resting = credential({ cooldowns: [cooldown("credential_quota", 3600)], quota: weekly });
    expect(credentialHealth(resting)).toMatchObject({
      label: "Resting",
      summary: "The weekly limit is used up",
      action: explainReason("credential_quota").action,
    });
    expect(explainCooldown(cooldown("quota", 60), resting)).toEqual({
      ...explainReason("quota"),
      title: "The weekly limit is used up",
    });
    // Another reason, no window used up, or no reading: the reason's own words.
    expect(explainCooldown(cooldown("unauthorized", 60), resting)).toEqual(
      explainReason("unauthorized"),
    );
    const allowed = { ...weekly, signals: { "Anthropic-Ratelimit-Unified-7d-Status": "allowed" } };
    expect(
      credentialHealth(credential({ cooldowns: [cooldown("credential_quota", 60)], quota: allowed }))
        .summary,
    ).toBe("The account's quota is used up");
    expect(credentialHealth(credential({ cooldowns: [cooldown("credential_quota", 60)] })).summary).toBe(
      "The account's quota is used up",
    );
  });

  it("splits the cooldowns by scope, and knows when a reset would help", () => {
    const resting = credential({
      cooldowns: [
        cooldown("quota", 600),
        cooldown("transient_error", 20),
        cooldown("model_not_supported", 40, { scope: "model", model_key: "gpt-5" }),
      ],
    });
    expect(credentialCooldowns(resting).map((item) => item.reason)).toEqual([
      "transient_error",
      "quota",
    ]);
    expect(modelCooldowns(resting).map((item) => item.model_key)).toEqual(["gpt-5"]);
    expect(canReset(resting)).toBe(true);
    expect(canReset(credential({ cooldowns: null }))).toBe(false);
    expect(canReset(credential({ unavailable: true }))).toBe(true);
    expect(canReset(credential({ next_retry_after: "2026-10-05T12:30:00Z" }))).toBe(true);
  });
});

describe("the sign-in messages", () => {
  it("explain each failure the server reports", () => {
    expect(explainSignInError("unknown or expired state").title).toBe("This sign-in has ended");
    expect(explainSignInError("Timeout waiting for OAuth callback").title).toBe(
      "The sign-in waited too long",
    );
    expect(explainSignInError("Bad request").title).toBe("The provider reported an error");
    expect(explainSignInError("Bad Request").title).toBe("The provider reported an error");
    expect(explainSignInError("Timeout exchanging authorization code for tokens").title).toBe(
      "The provider took too long",
    );
    const exchange = explainSignInError(
      "Failed to exchange authorization code for tokens: token endpoint said 400",
    );
    expect(exchange.title).toBe("The provider didn't accept the sign-in");
    expect(exchange.meaning).toContain("token endpoint said 400");
    expect(explainSignInError("Failed to save authentication tokens: no email").title).toBe(
      "The server couldn't save the credential",
    );
    expect(explainSignInError("code or error is required").title).toBe(
      "That isn't the address this sign-in sent you to",
    );
    expect(explainSignInError("provider does not match state").title).toBe(
      "That address is from another provider's sign-in",
    );
    expect(explainSignInError("oauth flow is already completed").title).toBe(
      "This sign-in has already finished",
    );
    expect(explainSignInError("Authentication failed").meaning).toBe("The server didn't say why.");
    expect(explainSignInError("something new").meaning).toBe("something new");
  });

  it("explain why a sign-in couldn't start, and when starting without the callback helps", () => {
    const port = explainStartError("claude", 500, "failed to start callback server");
    expect(port?.pasteOnly).toBe(true);
    expect(port?.reason.meaning).toContain("port 54545");
    expect(explainStartError("codex", 500, "callback server unavailable")?.reason.meaning).toContain(
      "port 1455",
    );
    expect(explainStartError("codex", 429, "too many oauth sessions")).toMatchObject({
      pasteOnly: false,
      reason: { title: "Too many sign-ins are open" },
    });
    expect(explainStartError("claude", 503, "server shutting down")?.reason.title).toBe(
      "The server is shutting down",
    );
    expect(explainStartError("claude", 500, "failed to generate PKCE codes")).toBeNull();
  });

  it("check a pasted address belongs to the sign-in", () => {
    const state = "s-123";
    expect(
      pastedAddressProblem("http://localhost:54545/callback?code=abc&state=s-123", state),
    ).toBeNull();
    expect(
      pastedAddressProblem("http://localhost:1455/auth/callback?error=access_denied&state=s-123", state),
    ).toBeNull();
    expect(pastedAddressProblem("abc", state)).toMatch(/^That isn't a web address/);
    expect(pastedAddressProblem("http://localhost:54545/callback?code=abc&state=other", state)).toMatch(
      /^That address isn't from this sign-in/,
    );
    expect(pastedAddressProblem("http://localhost:54545/callback?state=s-123", state)).toMatch(
      /^That address has no sign-in code/,
    );
  });
});

describe("the small words", () => {
  it("name the providers as people know them", () => {
    expect(providerName("claude")).toBe("Claude");
    expect(providerName("anthropic")).toBe("Claude");
    expect(providerName("codex")).toBe("Codex");
    expect(providerName("vertex")).toBe("Vertex AI");
    expect(providerName("")).toBe("Unknown provider");
    expect(providerName("openrouter")).toBe("openrouter");
  });

  it("round the time left to whole minutes", () => {
    expect(timeLeft(5)).toBe("less than a minute");
    expect(timeLeft(59)).toBe("less than a minute");
    expect(timeLeft(89)).toBe("about 1 min");
    expect(timeLeft(300)).toBe("about 5 min");
    expect(timeLeft(7290)).toBe("about 2 h 2 min");
  });
});
