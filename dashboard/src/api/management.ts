// The parts of CLIProxyAPI's management API the screens use, with the
// shapes upstream v8.0.20 answers with.

export const MANAGEMENT = "/v0/management";

/** `{"usage-statistics-enabled": bool}`; PUT `{"value": bool}`. */
export const USAGE_STATISTICS_ENABLED = `${MANAGEMENT}/usage-statistics-enabled`;
/** `{"request-log": bool}`; PUT `{"value": bool}`. */
export const REQUEST_LOG_SETTING = `${MANAGEMENT}/request-log`;
/** `{"logging-to-file": bool}`; PUT `{"value": bool}`. */
export const LOGGING_TO_FILE = `${MANAGEMENT}/logging-to-file`;
/** The server's own log, main.log and its rotations. */
export const SERVER_LOGS = `${MANAGEMENT}/logs`;
/** `{"api-keys": [...]}`: the client API keys. */
export const API_KEYS = `${MANAGEMENT}/api-keys`;
/** The config as JSON, with every setting under its config.yaml key. */
export const CONFIG = `${MANAGEMENT}/config`;
/**
 * config.yaml as it is on disk. `PUT` with the new file as the body saves
 * it, if the server can use it, and answers `{"ok": true, "changed": [...]}`.
 */
export const CONFIG_YAML = `${MANAGEMENT}/config.yaml`;

/** An answer of `GET /logs`. */
export interface ServerLogPage {
  lines: string[];
  "line-count": number;
  /** Unix seconds of the newest line read. */
  "latest-timestamp": number;
  /** Where to read on from, with `cursor`; empty when there is no log. */
  "next-cursor": string;
  /** The cursor was no longer good (the log rotated or was cleared): these
   * lines are the log's end again, not a continuation. */
  "cursor-reset"?: boolean;
}

/** `GET /logs` answers this while logging to a file is off. */
export const LOGGING_TO_FILE_DISABLED = "logging to file disabled";
