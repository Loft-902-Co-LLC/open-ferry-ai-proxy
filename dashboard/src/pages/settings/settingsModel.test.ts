import { describe, expect, it, vi } from "vitest";

import { ApiError } from "../../api/client";
import {
  checkAfterProblem,
  describeSetting,
  formValuesOf,
  isEdited,
  loadedProblems,
  preferenceOf,
  proxyUrlProblem,
  readEdited,
  readServerFacts,
  redactProxyUrl,
  saveCall,
  serverFactsOf,
  settingChanges,
  settingProblem,
  settingValuesOf,
  settingsSchema,
  strategyOf,
  type ServerFacts,
  type SettingValues,
} from "./settingsModel";

const DEFAULTS: SettingValues = {
  proxyUrl: "",
  routingStrategy: "round-robin",
  quotaPrefer: "soonest-reset",
  quotaReservePercent: 0,
  quotaCheckAfter: "",
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
  selfUpdateMode: "auto",
  managementAddress: "",
};

const FACTS: ServerFacts = { separateAddress: "127.0.0.1:8318", allowRemote: false, proxyPort: 8317 };

describe("settingValuesOf", () => {
  it("reads each setting from the config's JSON, and the management address from the facts", () => {
    expect(
      settingValuesOf(
        {
          "proxy-url": " http://proxy.example:3128 ",
          routing: {
            strategy: "Quota",
            quota: { prefer: " Most-Left ", "reserve-percent": 15, "check-after": " 1h30m " },
          },
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
          "self-update": { mode: " Notify ", "check-every": "12h" },
          "api-keys": ["sk-not-a-setting-here"],
        },
        FACTS,
      ),
    ).toEqual({
      proxyUrl: "http://proxy.example:3128",
      routingStrategy: "quota",
      quotaPrefer: "most-left",
      quotaReservePercent: 15,
      quotaCheckAfter: "1h30m",
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
      selfUpdateMode: "notify",
      managementAddress: "127.0.0.1:8318",
    });
  });

  it("reads the updates mode as the server loads it", () => {
    const mode = (value: unknown) => settingValuesOf({ "self-update": value }).selfUpdateMode;
    expect(mode({ mode: "OFF" })).toBe("off");
    expect(mode({ mode: "auto" })).toBe("auto");
    expect(mode({ mode: "" })).toBe("auto");
    expect(mode({ "check-every": "12h" })).toBe("auto");
    expect(mode(undefined)).toBe("auto");
  });

  it("reads the quota settings as the server loads them", () => {
    const quota = (value: unknown) => settingValuesOf({ routing: { quota: value } });
    expect(quota({ "reserve-percent": 150 }).quotaReservePercent).toBe(100);
    expect(quota({ "reserve-percent": -5 }).quotaReservePercent).toBe(0);
    expect(quota({ "reserve-percent": 12.9 }).quotaReservePercent).toBe(12);
    expect(quota({ prefer: "least-used" }).quotaPrefer).toBe("soonest-reset");
    expect(quota({ "check-after": 5 }).quotaCheckAfter).toBe("");
    expect(quota("off")).toEqual(DEFAULTS);
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
    expect(strategyOf(" QUOTA ")).toBe("quota");
    expect(strategyOf("by-quota")).toBe("round-robin");
    expect(strategyOf("")).toBe("round-robin");
    expect(strategyOf(undefined)).toBe("round-robin");
  });
});

describe("preferenceOf", () => {
  it("takes most-left in any case, and anything else as soonest-reset", () => {
    expect(preferenceOf(" MOST-LEFT ")).toBe("most-left");
    expect(preferenceOf("soonest-reset")).toBe("soonest-reset");
    expect(preferenceOf("mostleft")).toBe("soonest-reset");
    expect(preferenceOf(undefined)).toBe("soonest-reset");
  });
});

describe("checkAfterProblem", () => {
  it("takes empty for off, and a time the server reads", () => {
    for (const value of ["", "  ", "1h", " 90m ", "1h30m", "1.5h", "0", "0s", "500ms"]) {
      expect(checkAfterProblem(value), value).toBeNull();
    }
  });

  it("says how to write a time the server can't read, or a negative one", () => {
    for (const value of ["1d", "1 h", "soon", "60", "1H", "3000000h"]) {
      expect(checkAfterProblem(value), value).toMatch(
        /^The server can't read this as a time, and takes it as off\. Write a number and a unit/,
      );
    }
    expect(checkAfterProblem("-1h")).toBe(
      "A negative time is off. Leave it empty for off, or write a time such as 1h.",
    );
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

  it("checks the quota settings and the management address as typed", () => {
    const read = readEdited(
      {
        ...formValuesOf(DEFAULTS),
        quotaReservePercent: "101",
        quotaCheckAfter: "1 day",
        managementAddress: "http://127.0.0.1:8318",
      },
      DEFAULTS,
      FACTS,
    );
    expect(Object.keys(read.problems ?? {})).toEqual([
      "quotaReservePercent",
      "quotaCheckAfter",
      "managementAddress",
    ]);
    expect(read.problems?.quotaReservePercent).toBe(
      "The share kept back is a whole number from 0 to 100.",
    );
    expect(read.problems?.quotaCheckAfter).toMatch(/^The server can't read this as a time/);
    expect(read.problems?.managementAddress).toBe(
      "Leave out the scheme and any path: write host:port, such as 127.0.0.1:8318.",
    );
    const good = readEdited(
      { ...formValuesOf(DEFAULTS), quotaReservePercent: " 10 ", managementAddress: " :8318 " },
      DEFAULTS,
      FACTS,
    );
    expect(good.values).toEqual({ ...DEFAULTS, quotaReservePercent: 10, managementAddress: ":8318" });
  });

  it("refuses a management address on the proxy's port once it knows the port", () => {
    const input = { ...formValuesOf(DEFAULTS), managementAddress: "localhost:8317" };
    expect(readEdited(input, DEFAULTS, FACTS).problems).toEqual({
      managementAddress:
        "Port 8317 is the proxy's own (server.port). Pick another: the management address needs a port of its own.",
    });
    expect(readEdited(input, DEFAULTS).values?.managementAddress).toBe("localhost:8317");
    expect(settingProblem("managementAddress", "localhost:8317", FACTS)).toMatch(/^Port 8317/);
    // One in config.yaml already, left alone, is a warning.
    const clash = { ...DEFAULTS, managementAddress: "127.0.0.1:8317" };
    const problems = loadedProblems(clash, FACTS);
    expect(Object.keys(problems)).toEqual(["managementAddress"]);
    expect(problems.managementAddress).toMatch(/^Port 8317 is the proxy's own/);
    expect(readEdited(formValuesOf(clash), clash, FACTS).values).toEqual(clash);
  });
});

describe("saveCall", () => {
  it("saves upstream's settings by PATCH with the value wrapped, and open-ferry's by a v8 PUT", () => {
    expect(saveCall("requestRetry", 3)).toEqual({
      path: "/v0/management/request-retry",
      request: { method: "PATCH", json: { value: 3 } },
    });
    expect(saveCall("routingStrategy", "quota")).toEqual({
      path: "/v0/management/routing/strategy",
      request: { method: "PATCH", json: { value: "quota" } },
    });
    expect(saveCall("quotaPrefer", "most-left")).toEqual({
      path: "/v8/management/config/routing/quota/prefer",
      request: { method: "PUT", json: "most-left" },
    });
    expect(saveCall("quotaReservePercent", 10)).toEqual({
      path: "/v8/management/config/routing/quota/reserve-percent",
      request: { method: "PUT", json: 10 },
    });
    expect(saveCall("quotaCheckAfter", "")).toEqual({
      path: "/v8/management/config/routing/quota/check-after",
      request: { method: "PUT", json: "" },
    });
    expect(saveCall("managementAddress", "127.0.0.1:8318")).toEqual({
      path: "/v8/management/config/management/separate-address",
      request: { method: "PUT", json: "127.0.0.1:8318" },
    });
    expect(saveCall("selfUpdateMode", "off")).toEqual({
      path: "/v8/management/config/self-update/mode",
      request: { method: "PUT", json: "off" },
    });
  });
});

describe("serverFactsOf", () => {
  it("reads what the v8 config route answered, with defaults for what it didn't", () => {
    expect(
      serverFactsOf({ separateAddress: " :8318 ", allowRemote: true, proxyPort: 9000 }),
    ).toEqual({ separateAddress: ":8318", allowRemote: true, proxyPort: 9000 });
    expect(serverFactsOf({})).toEqual({ separateAddress: "", allowRemote: false, proxyPort: 0 });
    expect(serverFactsOf({ separateAddress: 8318, allowRemote: "true", proxyPort: "8317" })).toEqual(
      { separateAddress: "", allowRemote: false, proxyPort: 0 },
    );
  });
});

describe("readServerFacts", () => {
  it("reads them from the whole file in one call", async () => {
    const call = vi.fn<(path: string) => Promise<unknown>>(() =>
      Promise.resolve({
        "config-version": 8,
        server: { port: 8317, tls: { enable: false } },
        management: { "allow-remote": true, "separate-address": " 127.0.0.1:8318 " },
        "api-keys": { codex: ["sk-not-kept"] },
      }),
    );
    await expect(readServerFacts(call)).resolves.toEqual({
      separateAddress: "127.0.0.1:8318",
      allowRemote: true,
      proxyPort: 8317,
    });
    expect(call.mock.calls.map(([path]) => path)).toEqual(["/v8/management/config"]);
  });

  it("takes what the file doesn't have as unset", async () => {
    await expect(readServerFacts(() => Promise.resolve({ "config-version": 8 }))).resolves.toEqual({
      separateAddress: "",
      allowRemote: false,
      proxyPort: 0,
    });
    await expect(
      readServerFacts(() => Promise.resolve({ management: "on", server: null })),
    ).resolves.toEqual({ separateAddress: "", allowRemote: false, proxyPort: 0 });
  });

  it("fails on any other failure, such as a server without the route", async () => {
    const unsupported = new ApiError(404, null, null, null);
    await expect(readServerFacts(() => Promise.reject(unsupported))).rejects.toBe(unsupported);
    const broken = new ApiError(500, "read_failed", null, null);
    await expect(readServerFacts(() => Promise.reject(broken))).rejects.toBe(broken);
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

  it("says what the quota settings and the management address mean", () => {
    expect(describeSetting("routingStrategy", "quota")).toBe("By quota");
    expect(describeSetting("quotaPrefer", "most-left")).toBe("The most quota left");
    expect(describeSetting("quotaPrefer", "soonest-reset")).toBe("The limit that resets soonest");
    expect(describeSetting("quotaReservePercent", 0)).toBe("None");
    expect(describeSetting("quotaReservePercent", 10)).toBe("10%");
    expect(describeSetting("quotaCheckAfter", "")).toBe("Off");
    expect(describeSetting("quotaCheckAfter", "1h30m")).toBe("1h30m");
    expect(describeSetting("quotaCheckAfter", "0s")).toBe("Off (0s)");
    expect(describeSetting("quotaCheckAfter", "-1h")).toBe("Off (-1h)");
    expect(describeSetting("quotaCheckAfter", "1d")).toBe("Off (1d isn't a time)");
    expect(describeSetting("managementAddress", "")).toBe("None: on the proxy's port");
    expect(describeSetting("managementAddress", "[::1]:8318")).toBe("[::1]:8318");
  });

  it("says what each updates mode is", () => {
    expect(describeSetting("selfUpdateMode", "auto")).toBe("On");
    expect(describeSetting("selfUpdateMode", "notify")).toBe("Notify only");
    expect(describeSetting("selfUpdateMode", "off")).toBe("Off");
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
