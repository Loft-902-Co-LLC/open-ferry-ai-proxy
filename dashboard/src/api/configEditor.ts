// A form owns one version of config.yaml. Reads never replace that version:
// a background refresh or a review must not silently authorize overwriting
// someone else's edit. Only a successful write advances it, from its ETag.
import { ApiError, isUnsupportedRoute, type ApiRequest, type ApiResponse } from "./client";
import { API_KEYS, CONFIG, CONFIG_YAML, MANAGEMENT, V8_CONFIG } from "./management";
import { AUTH_FILE_STATUS, type Credential, type StatusAnswer } from "./credentials";

type ResponseCall = <T>(path: string, request?: ApiRequest) => Promise<ApiResponse<T>>;

const V8_YAML = "/v8/management/config.yaml";
const SETTINGS: Record<string, string> = {
  "proxy-url": "requests/proxy-url",
  "routing/strategy": "routing/strategy",
  "request-retry": "routing/retry/request-retry",
  "max-retry-credentials": "routing/retry/max-retry-credentials",
  "max-retry-interval": "routing/retry/max-retry-interval",
  "force-model-prefix": "routing/force-model-prefix",
  ...Object.fromEntries([
    "debug", "logging-to-file", "logs-max-total-size-mb", "request-log", "error-logs-max-files",
  ].map((name) => [name, `observability/logs/${name}`])),
  "usage-statistics-enabled": "observability/usage/usage-statistics-enabled",
};

function record(value: unknown): Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
    ? value as Record<string, unknown> : {};
}

function valueAt(tree: unknown, path: string): unknown {
  return path.split("/").reduce<unknown>((value, key) => record(value)[key], tree);
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.entries(record(value)).sort(([a], [b]) => a.localeCompare(b))
      .map(([key, item]) => [key, canonical(item)] as const));
  }
  return value;
}

function writable(value: unknown): Record<string, unknown> {
  const entry = { ...record(value) };
  delete entry["auth-index"];
  delete entry.auth_index;
  return entry;
}

const PROVIDER_LIST = /\/(claude|codex|gemini)-api-key$/;

/** The shapes the forms already use, projected from the on-disk v8 read.
 * Legacy reads can lag behind disk until the file watcher runs, so they
 * must never be paired with the newer file's ETag. Missing scalar fields
 * use the form's defaults, which match Config::default (counts 0, flags
 * false, error-log count 10, and round-robin routing). */
function formValues(tree: unknown, path: string): unknown {
  if (path === CONFIG) {
    return {
      ...Object.fromEntries(Object.entries(SETTINGS).map(([key, location]) =>
        [key, valueAt(tree, location)] as const)),
      routing: record(tree).routing,
      tls: record(record(tree).server).tls,
      "self-update": record(tree)["self-update"],
    };
  }
  if (path === API_KEYS) {
    return { "api-keys": valueAt(tree, "access/api-keys") ?? [] };
  }
  const family = PROVIDER_LIST.exec(path)?.[1] ?? "";
  const groups = record(record(tree)["api-keys"])[family];
  return { [`${family}-api-key`]: Array.isArray(groups) ? groups.flatMap((item: unknown) => {
    const group = { ...record(item) };
    const keys = group.keys;
    delete group.keys;
    return Array.isArray(keys) ? keys.map((key: unknown) => {
      const entry = { ...group, ...record(key) };
      if (entry.auth_index !== undefined) entry["auth-index"] = entry.auth_index;
      delete entry.auth_index;
      return entry;
    }) : [];
  }) : [] };
}

/** One form's on-disk config version. A read made after the version read
 * can only be newer: a save then conservatively conflicts, never pairs
 * an older in-memory value with a newer on-disk tag. */
export class ConfigEditor {
  private version: Promise<string | undefined> | undefined;
  private etag: string | undefined;

  constructor(private readonly response: ResponseCall) {}

  private loadVersion(): Promise<string | undefined> {
    this.version ??= this.response(V8_CONFIG).then((answer) => {
      this.etag = answer.headers.get("etag") ?? undefined;
      return this.etag;
    }).catch((error: unknown) => {
      if (isUnsupportedRoute(error)) return undefined;
      this.version = undefined;
      throw error;
    });
    return this.version;
  }

  private checkRead(answer: ApiResponse, condition: string): void {
    if (answer.headers.get("etag") !== condition) {
      throw new ApiError(412, "precondition_failed", null, null);
    }
  }

  private async snapshot(condition: string, signal?: AbortSignal): Promise<unknown> {
    const answer = await this.response(V8_CONFIG, { signal });
    this.checkRead(answer, condition);
    return answer.data;
  }

  readonly ready = () => this.loadVersion();

  /** Config-backed credential toggles also save config.yaml. Resolve the
   * credential by the v8 read's stable auth index, and retain every other
   * group/key field. Never send this write to the unguarded status route. */
  readonly toggleCredential = async (
    credential: Pick<Credential, "name" | "auth_index">,
    disabled: boolean,
  ): Promise<StatusAnswer> => {
    await this.loadVersion();
    if (this.etag === undefined) {
      return (await this.response<StatusAnswer>(AUTH_FILE_STATUS, {
        method: "PATCH", json: { name: credential.name, auth_index: credential.auth_index, disabled },
      })).data;
    }
    const condition = this.etag;
    const snapshot = await this.snapshot(condition);
    const families = record(record(snapshot)["api-keys"]);
    const changed: Record<string, unknown> = {};
    const matches = (entry: Record<string, unknown>) => credential.auth_index !== "" &&
      (entry.auth_index ?? entry["auth-index"]) === credential.auth_index;
    const excluded = (value: unknown): string[] => {
      const patterns = Array.isArray(value)
        ? value.filter((item): item is string => typeof item === "string" && item.trim() !== "*")
        : [];
      return disabled ? [...patterns, "*"] : patterns;
    };
    for (const [family, value] of Object.entries(families)) {
      if (!Array.isArray(value)) continue;
      const candidates = value.flatMap((item: unknown) => {
        const group = record(item);
        return [group, ...(Array.isArray(group.keys) ? group.keys.map(record) : [])];
      });
      if (!candidates.some(matches)) continue;
      const list = family === "openai-compatibility" ? family : `${family}-api-key`;
      const current = await this.response<Record<string, unknown>>(`${MANAGEMENT}/${list}`);
      const listed = current.data[list];
      const live = Array.isArray(listed) ? listed.flatMap((item: unknown) => {
        const group = { ...record(item) };
        const keys = group["api-key-entries"];
        delete group["api-key-entries"];
        return Array.isArray(keys) && keys.length > 0
          ? keys.map((key: unknown) => ({ ...group, ...record(key) })) : [group];
      }).find(matches) : undefined;
      const identity = (entry: Record<string, unknown>) => JSON.stringify([
        ...[...(family === "openai-compatibility" ? ["name"] : []), "api-key", "base-url", "prefix", "proxy-url"].map((key) => typeof entry[key] === "string" ? entry[key].trim() : ""),
        Object.entries(record(entry.headers)).map(([key, value]) => [key, String(value)]).sort(),
      ]);
      const sameTarget = (entry: Record<string, unknown>) => matches(entry) &&
        live !== undefined && identity(entry) === identity(live);
      const match = { found: false };
      const groups = value.map((item: unknown) => {
        const group = { ...record(item) };
        if (sameTarget(group)) {
          group["excluded-models"] = excluded(group["excluded-models"]);
          match.found = true;
        }
        if (Array.isArray(group.keys)) {
          group.keys = group.keys.map((item: unknown) => {
            const key = { ...record(item) };
            if (sameTarget({ ...group, ...key })) {
              key["excluded-models"] = excluded(key["excluded-models"] ?? group["excluded-models"]);
              match.found = true;
            }
            return key;
          });
        }
        return group;
      });
      if (match.found) changed[family] = groups;
    }
    if (Object.keys(changed).length === 0) {
      // The displayed credential no longer names a key in the current file.
      throw new ApiError(412, "precondition_failed", null, null);
    }
    await this.call(V8_CONFIG, { method: "PATCH", json: { "api-keys": changed }, ifMatch: condition });
    return { status: "ok", disabled };
  };

  readonly call = async <T>(path: string, request: ApiRequest = {}): Promise<T> => {
    await this.loadVersion();
    const writing = request.method !== undefined && request.method !== "GET";
    // Older endpoints remain usable, without inventing an If-Match value.
    if (this.etag === undefined) return (await this.response<T>(path, request)).data;
    const condition = request.ifMatch ?? this.etag;
    if (!writing && (path === CONFIG || path === API_KEYS || PROVIDER_LIST.test(path))) {
      return formValues(await this.snapshot(condition, request.signal), path) as T;
    }
    let target = path;
    let change = request;
    if (path === CONFIG_YAML) {
      target = V8_YAML;
    } else if (writing && path.startsWith(`${MANAGEMENT}/`)) {
      const setting = SETTINGS[path.slice(MANAGEMENT.length + 1)];
      if (setting !== undefined) {
        target = `${V8_CONFIG}/${setting}`;
        change = { method: "PUT", json: record(request.json).value };
      } else if (path === API_KEYS) {
        // Index deletes and read/modify/write additions are guarded by the
        // form's version, so another writer cannot move a different key here.
        const answer = formValues(await this.snapshot(condition), API_KEYS) as { "api-keys": string[] | null };
        const keys = [...(answer["api-keys"] ?? [])];
        if (request.method === "DELETE") {
          const index = Number(request.query?.index);
          if (!Number.isInteger(index) || request.configTarget === undefined || keys[index] !== request.configTarget.key) {
            throw new ApiError(412, "precondition_failed", null, null);
          }
          keys.splice(index, 1);
        } else if (request.method === "PATCH") {
          const body = record(request.json);
          const old = typeof body.old === "string" ? body.old : "";
          const key = typeof body.new === "string" ? body.new : "";
          const at = keys.indexOf(old);
          if (at < 0) keys.push(key);
          else keys[at] = key;
        }
        target = `${V8_CONFIG}/access/api-keys`;
        change = { method: "PUT", json: request.method === "PUT" ? request.json : keys };
      } else if (PROVIDER_LIST.test(path)) {
        const family = PROVIDER_LIST.exec(path)?.[1] ?? "";
        const snapshot = await this.snapshot(condition);
        const stored = record(record(snapshot)["api-keys"])[family];
        const groups: unknown[] = Array.isArray(stored) ? stored.map((item: unknown) => item) : [];
        const existing = record(formValues(snapshot, path))[`${family}-api-key`];
        const entries: unknown[] = Array.isArray(existing) ? existing : [];
        if (request.method === "DELETE") {
          const index = Number(request.query?.index);
          const entry = record(entries[index]);
          const intended = request.configTarget;
          if (!Number.isInteger(index) || intended === undefined || entry["api-key"] !== intended.key ||
              (entry["base-url"] ?? "") !== (intended.baseUrl ?? "")) {
            throw new ApiError(412, "precondition_failed", null, null);
          }
          let place = 0;
          for (let i = 0; i < groups.length; i++) {
            const group = record(groups[i]);
            const keys: unknown[] = Array.isArray(group.keys) ? group.keys : [];
            if (index >= place && index < place + keys.length) {
              groups[i] = { ...group, keys: keys.filter((_, at) => at !== index - place) };
              break;
            }
            place += keys.length;
          }
        } else {
          const next: unknown[] = Array.isArray(request.json) ? request.json : [];
          if (next.length !== entries.length + 1 ||
              entries.some((entry, index) => JSON.stringify(canonical(writable(entry))) !== JSON.stringify(canonical(writable(next[index]))))) {
            throw new ApiError(412, "precondition_failed", null, null);
          }
          const key = writable(next.at(-1));
          const baseUrl = key["base-url"];
          delete key["base-url"];
          groups.push({ ...(baseUrl === undefined ? {} : { "base-url": baseUrl }), keys: [key] });
        }
        target = `${V8_CONFIG}/api-keys/${family}`;
        change = { method: "PUT", json: groups };
      } else {
        // A newly added writer must be mapped deliberately, never silently
        // fall back to an endpoint that ignores the precondition.
        throw new Error("This config write has no versioned route.");
      }
    }
    const answer = await this.response<T>(target, {
      ...change, ...(writing ? { ifMatch: request.ifMatch ?? condition } : {}),
    });
    if (!writing) this.checkRead(answer, condition);
    if (writing) {
      const next = answer.headers.get("etag");
      // If a guarded server drops its tag, fail closed on the next write.
      if (next !== null) this.etag = next;
    }
    return answer.data;
  };
}
