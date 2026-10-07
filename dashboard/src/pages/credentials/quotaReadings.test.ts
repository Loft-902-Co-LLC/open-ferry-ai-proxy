import { describe, expect, it } from "vitest";

import { credential } from "../../test/fixtures";
import { limitUsedUp, quotaReadings } from "./quotaReadings";

const OBSERVED = "2026-10-05T11:58:00.000Z";

function claude(signals: Record<string, string>) {
  return credential({ quota: { observed_at: OBSERVED, signals } });
}

function codex(signals: Record<string, string>) {
  return credential({ provider: "codex", quota: { observed_at: OBSERVED, signals } });
}

describe("a credential's quota readings", () => {
  it("read Claude's 5-hour and weekly windows", () => {
    const readings = quotaReadings(
      claude({
        "Anthropic-Ratelimit-Unified-5h-Status": "allowed",
        "Anthropic-Ratelimit-Unified-5h-Utilization": "0.25",
        "Anthropic-Ratelimit-Unified-5h-Reset": "1791216000",
        "Anthropic-Ratelimit-Unified-7d-Status": "allowed_warning",
        "Anthropic-Ratelimit-Unified-7d-Utilization": "0.53",
        "Anthropic-Ratelimit-Unified-7d-Reset": "1791648000",
        "Anthropic-Ratelimit-Unified-Representative-Claim": "five_hour",
      }),
    );
    expect(readings).toEqual({
      observedAt: new Date(OBSERVED),
      windows: [
        { name: "5-hour", used: 0.25, resetsAt: new Date(1791216000 * 1000), usedUp: false },
        { name: "Weekly", used: 0.53, resetsAt: new Date(1791648000 * 1000), usedUp: false },
      ],
      limiting: null,
    });
  });

  it("name the Claude window used up, the one Claude names first", () => {
    const rejected = {
      "Anthropic-Ratelimit-Unified-5h-Status": "rejected",
      "Anthropic-Ratelimit-Unified-5h-Reset": "1791216000",
      "Anthropic-Ratelimit-Unified-7d-Status": "rejected",
      "Anthropic-Ratelimit-Unified-7d-Reset": "1791648000",
    };
    const named = quotaReadings(
      claude({ ...rejected, "Anthropic-Ratelimit-Unified-Representative-Claim": "five_hour" }),
    );
    expect(named?.limiting?.name).toBe("5-hour");
    // Without a claim, the one that starts over last.
    expect(quotaReadings(claude(rejected))?.limiting?.name).toBe("Weekly");
    const full =
      quotaReadings(claude({ "anthropic-ratelimit-unified-7d-utilization": "1.0" }))?.limiting ?? null;
    expect(full).toEqual({ name: "Weekly", used: 1, resetsAt: null, usedUp: true });
    expect(full === null ? null : limitUsedUp(full)).toBe("The weekly limit is used up");
  });

  it("read Codex's windows by their length, with resets after the response", () => {
    const readings = quotaReadings(
      codex({
        "X-Codex-Plan-Type": "pro",
        "X-Codex-Primary-Used-Percent": "100",
        "X-Codex-Primary-Window-Minutes": "300",
        "X-Codex-Primary-Reset-After-Seconds": "3600",
        "X-Codex-Secondary-Used-Percent": "51",
        "X-Codex-Secondary-Window-Minutes": "10080",
        "X-Codex-Secondary-Reset-At": "1791648000",
        "X-Codex-Bengalfox-Primary-Used-Percent": "99",
      }),
    );
    expect(readings?.windows).toEqual([
      {
        name: "5-hour",
        used: 1,
        resetsAt: new Date(Date.parse(OBSERVED) + 3_600_000),
        usedUp: true,
      },
      { name: "Weekly", used: 0.51, resetsAt: new Date(1791648000 * 1000), usedUp: false },
    ]);
    expect(readings?.limiting?.name).toBe("5-hour");
    expect(
      quotaReadings(
        codex({
          "X-Codex-Primary-Used-Percent": "5",
          "X-Codex-Primary-Window-Minutes": "1440",
          "X-Codex-Secondary-Used-Percent": "7",
          "X-Codex-Secondary-Window-Minutes": "45",
        }),
      )?.windows.map((window) => window.name),
    ).toEqual(["Daily", "45-minute"]);
    expect(
      quotaReadings(codex({ "X-Codex-Secondary-Used-Percent": "7" }))?.windows.map(
        (window) => window.name,
      ),
    ).toEqual(["Secondary"]);
  });

  it("leave out what isn't a number or a time", () => {
    const readings = quotaReadings(
      codex({
        "X-Codex-Primary-Used-Percent": "<b>50</b>",
        "X-Codex-Primary-Window-Minutes": "-300",
        "X-Codex-Primary-Reset-At": "soon",
        "X-Codex-Secondary-Used-Percent": "1e3",
        "X-Codex-Secondary-Reset-At": "2026-10-09T08:00:00Z",
      }),
    );
    expect(readings?.windows).toEqual([
      {
        name: "Secondary",
        used: null,
        resetsAt: new Date("2026-10-09T08:00:00Z"),
        usedUp: false,
      },
    ]);
  });

  it("are none without a reading, or for another provider", () => {
    expect(quotaReadings(credential())).toBeNull();
    expect(quotaReadings(credential({ quota: { signals: {} } }))).toBeNull();
    expect(quotaReadings(claude({ "Retry-After": "30" }))).toBeNull();
    expect(
      quotaReadings(
        credential({
          provider: "gemini",
          quota: { observed_at: OBSERVED, signals: { "X-Codex-Primary-Used-Percent": "5" } },
        }),
      ),
    ).toBeNull();
    expect(
      quotaReadings(
        credential({
          quota: { observed_at: "never", signals: { "Anthropic-Ratelimit-Unified-5h-Status": "allowed" } },
        }),
      ),
    ).toBeNull();
  });
});
