import { MANAGEMENT, V8_CONFIG } from "./management";

export interface SettingInfo {
  /** Its name, as the review lists it. */
  label: string;
  /** Its key in config.yaml. */
  configKey: string;
  /**
   * The route that changes it: a v0 route, which takes `{"value": ...}` by
   * PATCH, or with `v8`, a v8 config path, which takes the bare value by PUT.
   */
  path: string;
  v8?: true;
}

export const CONFIG_FIELDS = {
  proxyUrl: { label: "Proxy", configKey: "proxy-url", path: `${MANAGEMENT}/proxy-url` },
  routingStrategy: {
    label: "How credentials are picked",
    configKey: "routing.strategy",
    path: `${MANAGEMENT}/routing/strategy`,
  },
  quotaPrefer: {
    label: "Quota preference",
    configKey: "routing.quota.prefer",
    path: `${V8_CONFIG}/routing/quota/prefer`,
    v8: true,
  },
  quotaReservePercent: {
    label: "Quota kept back",
    configKey: "routing.quota.reserve-percent",
    path: `${V8_CONFIG}/routing/quota/reserve-percent`,
    v8: true,
  },
  quotaCheckAfter: {
    label: "Check quota rests after",
    configKey: "routing.quota.check-after",
    path: `${V8_CONFIG}/routing/quota/check-after`,
    v8: true,
  },
  requestRetry: { label: "Retries", configKey: "request-retry", path: `${MANAGEMENT}/request-retry` },
  maxRetryCredentials: {
    label: "Credentials per round",
    configKey: "max-retry-credentials",
    path: `${MANAGEMENT}/max-retry-credentials`,
  },
  maxRetryInterval: {
    label: "Longest wait for a retry",
    configKey: "max-retry-interval",
    path: `${MANAGEMENT}/max-retry-interval`,
  },
  forceModelPrefix: {
    label: "Prefixed credentials need the prefix",
    configKey: "force-model-prefix",
    path: `${MANAGEMENT}/force-model-prefix`,
  },
  debug: { label: "Debug logging", configKey: "debug", path: `${MANAGEMENT}/debug` },
  loggingToFile: {
    label: "Log to files",
    configKey: "logging-to-file",
    path: `${MANAGEMENT}/logging-to-file`,
  },
  logsMaxTotalSizeMb: {
    label: "Log directory limit",
    configKey: "logs-max-total-size-mb",
    path: `${MANAGEMENT}/logs-max-total-size-mb`,
  },
  requestLog: { label: "Request logs", configKey: "request-log", path: `${MANAGEMENT}/request-log` },
  errorLogsMaxFiles: {
    label: "Failed-request logs kept",
    configKey: "error-logs-max-files",
    path: `${MANAGEMENT}/error-logs-max-files`,
  },
  usageStatisticsEnabled: {
    label: "Usage statistics",
    configKey: "usage-statistics-enabled",
    path: `${MANAGEMENT}/usage-statistics-enabled`,
  },
  selfUpdateChannel: {
    label: "Update channel",
    configKey: "self-update.channel",
    path: `${V8_CONFIG}/self-update/channel`,
    v8: true,
  },
  selfUpdateMode: {
    label: "Updates",
    configKey: "self-update.mode",
    path: `${V8_CONFIG}/self-update/mode`,
    v8: true,
  },
  managementAddress: {
    label: "Management address",
    configKey: "management.separate-address",
    path: `${V8_CONFIG}/management/separate-address`,
    v8: true,
  },
} satisfies Record<string, SettingInfo>;
