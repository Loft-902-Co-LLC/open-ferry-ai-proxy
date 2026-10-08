import { describe, expect, it } from "vitest";

import {
  describeSetting,
  formValuesOf,
  isEdited,
  loadedProblems,
  proxyUrlProblem,
  readEdited,
  redactProxyUrl,
  settingChanges,
  settingValuesOf,
  settingsSchema,
  strategyOf,
  type SettingValues,
} from "./settingsModel";

const DEFAULTS: SettingValues = {
  proxyUrl: "",
  routingStrategy: "round-robin",
  requestRetry: 0,
  maxRetryCredentials: 0,
  maxRetryInterval: 0,
  forceModelPrefix: false,
  debug: false,
  loggingToFile: false,
  logsMaxTotalSizeMb: 0,
  requestLog: false,
  errorLogsMaxFiles: 10,
  usageStatisticsEnabled: false,
};

describe("settingValuesOf", () => {
  it("reads each setting from the config's JSON", () => {
    expect(
      settingValuesOf({
        "proxy-url": " http://proxy.example:3128 ",
        routing: { strategy: "fill-first" },
        "request-retry": 3,
        "max-retry-credentials": 2,
        "max-retry-interval": 30,
        "force-model-prefix": true,
        debug: true,
        "logging-to-file": true,
        "logs-max-total-size-mb": 512,
        "request-log": true,
        "error-logs-max-files": 0,
        "usage-statistics-enabled": true,
        "api-keys": ["sk-not-a-setting-here"],
      }),
    ).toEqual({
      proxyUrl: "http://proxy.example:3128",
      routingStrategy: "fill-first",
      requestRetry: 3,
      maxRetryCredentials: 2,
      maxRetryInterval: 30,
      forceModelPrefix: true,
      debug: true,
      loggingToFile: true,
      logsMaxTotalSizeMb: 512,
      requestLog: true,
      errorLogsMaxFiles: 0,
      usageStatisticsEnabled: true,
    });
  });

  it("reads what is missing or out of range as the server loads it", () => {
    expect(settingValuesOf({})).toEqual(DEFAULTS);
    expect(settingValuesOf(null)).toEqual(DEFAULTS);
    expect(
      settingValuesOf({
        "logs-max-total-size-mb": -5,
        "error-logs-max-files": -1,
        "max-retry-credentials": -2,
        "request-retry": 2.7,
        debug: "yes",
        routing: { strategy: "random" },
      }),
    ).toEqual({ ...DEFAULTS, requestRetry: 2 });
  });
});

describe("strategyOf", () => {
  it("takes the canonical and short names in any case, and anything else as round robin", () => {
    expect(strategyOf("WRR")).toBe("weighted-round-robin");
    expect(strategyOf(" weightedRoundRobin ")).toBe("weighted-round-robin");
    expect(strategyOf("ff")).toBe("fill-first");
    expect(strategyOf("FillFirst")).toBe("fill-first");
    expect(strategyOf("rr")).toBe("round-robin");
    expect(strategyOf("")).toBe("round-robin");
    expect(strategyOf(undefined)).toBe("round-robin");
  });
});

describe("proxyUrlProblem", () => {
  it("takes nothing, direct, none, and HTTP or HTTPS proxies", () => {
    for (const value of ["", "  ", "direct", "NONE", "http://proxy.example:3128", "https://user:pass@proxy.example"]) {
      expect(proxyUrlProblem(value), value).toBeNull();
    }
  });

  it("refuses what the providers' clients can't use", () => {
    expect(proxyUrlProblem("proxy.example:3128")).toMatch(/starts with http/);
    expect(proxyUrlProblem("socks5://proxy.example:1080")).toMatch(/SOCKS5/);
    expect(proxyUrlProblem("socks5h://proxy.example:1080")).toMatch(/SOCKS5/);
    expect(proxyUrlProblem("ftp://proxy.example")).toMatch(/starts with http/);
    expect(proxyUrlProblem("not a url")).toMatch(/Enter an address/);
  });

  it("hides a user name and password", () => {
    expect(redactProxyUrl("http://user:secret@proxy.example:3128/")).toBe("http://•••@proxy.example:3128/");
    expect(redactProxyUrl("http://proxy.example:3128")).toBe("http://proxy.example:3128");
  });
});

describe("settingsSchema", () => {
  it("reads the form's text as the values the routes take", () => {
    const parsed = settingsSchema.safeParse({
      ...formValuesOf(DEFAULTS),
      proxyUrl: " direct ",
      requestRetry: " 4 ",
    });
    expect(parsed.success && parsed.data).toEqual({ ...DEFAULTS, proxyUrl: "direct", requestRetry: 4 });
  });

  it("says what is wrong with a number", () => {
    const parsed = settingsSchema.safeParse({ ...formValuesOf(DEFAULTS), requestRetry: "-1" });
    expect(parsed.success).toBe(false);
    expect(parsed.error?.issues).toHaveLength(1);
    expect(parsed.error?.issues[0]?.message).toBe("Retries is a whole number, 0 or more.");
  });

  it("takes any count up to the largest whole number the page holds exactly", () => {
    const largest = String(Number.MAX_SAFE_INTEGER);
    const parsed = settingsSchema.safeParse({ ...formValuesOf(DEFAULTS), requestRetry: largest });
    expect(parsed.success && parsed.data.requestRetry).toBe(Number.MAX_SAFE_INTEGER);
    for (const tooLarge of ["9007199254740992", "1".padEnd(400, "0")]) {
      const refused = settingsSchema.safeParse({ ...formValuesOf(DEFAULTS), requestRetry: tooLarge });
      expect(refused.error?.issues.map((issue) => issue.message)).toEqual([
        "Retries can be at most 9,007,199,254,740,991.",
      ]);
    }
  });
});

describe("isEdited", () => {
  it("compares the form's text with the server's value", () => {
    expect(isEdited(" 5000 ", 5000)).toBe(false);
    expect(isEdited("05000", 5000)).toBe(false);
    expect(isEdited("5001", 5000)).toBe(true);
    expect(isEdited("five", 5000)).toBe(true);
    expect(isEdited("", 0)).toBe(true);
    expect(isEdited(" direct ", "direct")).toBe(false);
    expect(isEdited(true, false)).toBe(true);
  });
});

describe("readEdited", () => {
  const loaded: SettingValues = {
    ...DEFAULTS,
    proxyUrl: "socks5://proxy.example:1080",
    requestRetry: 2 ** 60,
  };

  it("leaves the settings not edited as they were loaded, unchecked", () => {
    expect(loadedProblems(loaded)).toEqual({
      proxyUrl: "The server can't use a SOCKS5 proxy yet. Use an HTTP or HTTPS proxy.",
      requestRetry: "Retries can be at most 9,007,199,254,740,991.",
    });
    const read = readEdited({ ...formValuesOf(loaded), debug: true }, loaded);
    expect(read.values).toEqual({ ...loaded, debug: true });
    expect(settingChanges(loaded, read.values ?? loaded, loaded).map((change) => change.id)).toEqual([
      "debug",
    ]);
  });

  it("checks the settings edited", () => {
    const read = readEdited(
      { ...formValuesOf(loaded), proxyUrl: "socks5h://other.example:1080", maxRetryInterval: "soon" },
      loaded,
    );
    expect(read.values).toBeNull();
    expect(read.problems).toEqual({
      proxyUrl: "The server can't use a SOCKS5 proxy yet. Use an HTTP or HTTPS proxy.",
      maxRetryInterval: "The longest wait is a whole number, 0 or more.",
    });
  });

  it("checks every setting with nothing loaded to compare with", () => {
    expect(readEdited(formValuesOf(loaded), undefined).problems).toEqual(loadedProblems(loaded));
  });
});

describe("describeSetting", () => {
  it("says what a value means", () => {
    expect(describeSetting("debug", true)).toBe("On");
    expect(describeSetting("requestRetry", 0)).toBe("None");
    expect(describeSetting("requestRetry", 1)).toBe("1 retry");
    expect(describeSetting("maxRetryCredentials", 0)).toBe("All of them");
    expect(describeSetting("maxRetryInterval", 90)).toBe("90 seconds");
    expect(describeSetting("logsMaxTotalSizeMb", 0)).toBe("No limit");
    expect(describeSetting("logsMaxTotalSizeMb", 2048)).toBe("2,048 MB");
    expect(describeSetting("errorLogsMaxFiles", 10)).toBe("10 files");
    expect(describeSetting("routingStrategy", "fill-first")).toBe("Fill first");
    expect(describeSetting("proxyUrl", "")).toBe("The environment's proxy, if any");
    expect(describeSetting("proxyUrl", "Direct")).toBe("No proxy");
    expect(describeSetting("proxyUrl", "http://a:b@proxy.example")).toBe("http://•••@proxy.example");
  });
});

describe("settingChanges", () => {
  it("sends what the user changed and the server doesn't have yet", () => {
    const loaded = { ...DEFAULTS, requestRetry: 1 };
    const edited = { ...loaded, debug: true, requestRetry: 3, maxRetryInterval: 5 };
    const fresh = { ...loaded, requestRetry: 2, maxRetryInterval: 5, requestLog: true };
    expect(settingChanges(loaded, edited, fresh)).toEqual([
      { id: "requestRetry", now: 2, after: 3, movedOnServer: true },
      { id: "debug", now: false, after: true, movedOnServer: false },
    ]);
  });
});
