// Shared reads follow the current file. Each dirty form retains its draft's
// baseline and compares only the fields it will write before rebasing.
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
  return path === "" ? tree : path.split("/").reduce<unknown>((value, key) => record(value)[key], tree);
}

function canonical(value: unknown): unknown {
  if (Array.isArray(value)) return value.map(canonical);
  if (value !== null && typeof value === "object") {
    return Object.fromEntries(Object.entries(record(value)).filter(([key]) => key !== "auth_index" && key !== "auth-index").sort(([a], [b]) => a.localeCompare(b))
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


interface Snapshot {
  tree: unknown;
  etag: string | undefined;
}
interface Change { path: string; value: unknown }

const FIELD_NAMES: Record<string, string> = {
  "routing/retry/request-retry": "Retries",
  "access/api-keys": "Client API keys",
  "api-keys": "Provider API keys",
  "api-keys/claude": "Claude API keys",
  "api-keys/codex": "Codex API keys",
  "api-keys/gemini": "Gemini API keys",
  "observability/usage/usage-statistics-enabled": "Usage statistics",
};
function fieldName(path: string): string {
  return FIELD_NAMES[path] ?? (path === "" ? "config.yaml" : path.replaceAll("/", "."));
}
function same(a: unknown, b: unknown): boolean {
  return JSON.stringify(canonical(a)) === JSON.stringify(canonical(b));
}
function changedTree(tree: unknown, changes: readonly Change[]): unknown {
  let result: unknown = structuredClone(tree);
  for (const { path, value } of changes) {
    if (path === "") { result = structuredClone(value); continue; }
    const root = { ...record(result) };
    let parent = root;
    const parts = path.split("/");
    for (const part of parts.slice(0, -1)) {
      const child = { ...record(parent[part]) };
      parent[part] = child;
      parent = child;
    }
    parent[parts.at(-1) ?? ""] = structuredClone(value);
    result = root;
  }
  return result;
}

/** A local refusal: these field names are safe to display, never values. */
export class ConfigConflict extends ApiError {
  constructor(readonly fields: readonly string[]) {
    super(412, "precondition_failed", null, null);
    this.name = "ConfigConflict";
  }
}

/** One authenticated API client's store. Successful writes refetch mounted
 * queries; editor baselines remain independent only while they have drafts. */
export class ConfigStore {
  private initial: Promise<Snapshot> | undefined;
  constructor(readonly response: ResponseCall, readonly changed: () => void = () => { /* No mounted queries in standalone adapters. */ }) {}

  readonly read = async (signal?: AbortSignal): Promise<Snapshot> => {
    const answer = await this.response(V8_CONFIG, { signal });
    return { tree: answer.data, etag: answer.headers.get("etag") ?? undefined };
  };
  readonly ready = (): Promise<Snapshot> => {
    this.initial ??= this.read().catch((error: unknown) => {
      if (isUnsupportedRoute(error)) return { tree: undefined, etag: undefined };
      this.initial = undefined;
      throw error;
    });
    return this.initial;
  };
}

export class ConfigEditor {
  private base: Snapshot | undefined;
  private dirty = false;
  readonly store: ConfigStore;

  constructor(store: ConfigStore | ResponseCall) {
    this.store = store instanceof ConfigStore ? store : new ConfigStore(store);
  }
  private get response(): ResponseCall { return this.store.response; }
  get versioned(): boolean { return this.base?.etag !== undefined; }
  readonly setDirty = (dirty: boolean) => { this.dirty = dirty; };
  readonly reset = () => { this.base = undefined; this.dirty = false; };
  readonly ready = async () => {
    this.base ??= await this.store.ready();
    return this.base.etag;
  };
  private adopt(snapshot: Snapshot): void {
    if (!this.dirty || this.base === undefined) this.base = snapshot;
  }
  private async read(signal?: AbortSignal): Promise<Snapshot> {
    const fresh = await this.store.read(signal);
    if (this.versioned && fresh.etag === undefined) {
      throw new Error("The server omitted the config version. Reload before saving.");
    }
    this.adopt(fresh);
    return fresh;
  }
  private async forWrite(paths: readonly string[], wholeFile = false): Promise<Snapshot> {
    const fresh = await this.read();
    const base = this.base;
    const conflicts = wholeFile && base?.etag !== fresh.etag ? ["config.yaml"] :
      paths.filter((path) => !same(valueAt(base?.tree, path), valueAt(fresh.tree, path))).map(fieldName);
    if (conflicts.length > 0) throw new ConfigConflict(conflicts);
    return fresh;
  }
  private paths(path: string): string[] {
    if (path === CONFIG_YAML) return [""];
    if (path === API_KEYS) return ["access/api-keys"];
    const family = PROVIDER_LIST.exec(path)?.[1];
    if (family !== undefined) return [`api-keys/${family}`];
    if (path.startsWith(`${V8_CONFIG}/`)) return [path.slice(V8_CONFIG.length + 1)];
    const setting = SETTINGS[path.slice(MANAGEMENT.length + 1)];
    if (path.startsWith(`${MANAGEMENT}/`) && setting !== undefined) return [setting];
    throw new Error("This config write has no versioned route.");
  }
  /** Check the entire pending Settings batch before showing its review. */
  readonly checkChanges = async (paths: readonly string[]) => {
    await this.ready();
    if (this.versioned && paths.length > 0) await this.forWrite(paths.flatMap((path) => this.paths(path)));
  };
  private async send<T>(path: string, request: ApiRequest, fresh: Snapshot, changes: readonly Change[]): Promise<T> {
    const answer = await this.response<T>(path, { ...request, ifMatch: fresh.etag });
    // A failed request never reaches here, never retries, and never adopts
    // the refusal's ETag. Advance only saved fields in a dirty baseline.
    this.base = {
      tree: changedTree(this.base?.tree, changes),
      etag: answer.headers.get("etag") ?? fresh.etag,
    };
    this.store.changed();
    return answer.data;
  }

  /** Config-backed credential toggles also save config.yaml. Resolve the
   * credential by the v8 read's stable auth index, and retain every other
   * group/key field. Never send this write to the unguarded status route. */
  readonly toggleCredential = async (
    credential: Pick<Credential, "name" | "auth_index">,
    disabled: boolean,
  ): Promise<StatusAnswer> => {
    await this.ready();
    if (this.base?.etag === undefined) {
      return (await this.response<StatusAnswer>(AUTH_FILE_STATUS, {
        method: "PATCH", json: { name: credential.name, auth_index: credential.auth_index, disabled },
      })).data;
    }
    const fresh = await this.forWrite(["api-keys"]);
    const snapshot = fresh.tree;
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
      throw new ConfigConflict(["Provider API keys"]);
    }
    await this.send(V8_CONFIG, { method: "PATCH", json: { "api-keys": changed } }, fresh,
      Object.entries(changed).map(([family, value]) => ({ path: `api-keys/${family}`, value })));
    return { status: "ok", disabled };
  };


  readonly call = async <T>(path: string, request: ApiRequest = {}): Promise<T> => {
    await this.ready();
    const writing = request.method !== undefined && request.method !== "GET";
    if (!this.versioned) {
      const answer = await this.response<T>(path, request);
      if (writing) this.store.changed();
      return answer.data;
    }
    if (!writing) {
      if (path === CONFIG_YAML) {
        const answer = await this.response<T>(V8_YAML, request);
        this.adopt({ tree: this.base?.tree, etag: answer.headers.get("etag") ?? undefined });
        return answer.data;
      }
      if (path === CONFIG || path === API_KEYS || PROVIDER_LIST.test(path) || path === V8_CONFIG) {
        const fresh = await this.read(request.signal);
        return (path === V8_CONFIG ? fresh.tree : formValues(fresh.tree, path)) as T;
      }
      return (await this.response<T>(path, request)).data;
    }
    const locations = this.paths(path);
    const fresh = await this.forWrite(locations, path === CONFIG_YAML);
    let target = path;
    let change = request;
    let value: unknown = request.json;
    if (path === CONFIG_YAML) {
      target = V8_YAML;
    } else if (path.startsWith(`${MANAGEMENT}/`)) {
      const setting = SETTINGS[path.slice(MANAGEMENT.length + 1)];
      if (setting !== undefined) {
        target = `${V8_CONFIG}/${setting}`;
        value = record(request.json).value;
        change = { method: "PUT", json: value };
      } else if (path === API_KEYS) {
        const answer = formValues(fresh.tree, API_KEYS) as { "api-keys": string[] | null };
        const keys = [...(answer["api-keys"] ?? [])];
        if (request.method === "DELETE") {
          const index = Number(request.query?.index);
          if (!Number.isInteger(index) || request.configTarget === undefined || keys[index] !== request.configTarget.key) {
            throw new ConfigConflict(["Client API keys"]);
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
        value = request.method === "PUT" ? request.json : keys;
        change = { method: "PUT", json: value };
      } else if (PROVIDER_LIST.test(path)) {
        const family = PROVIDER_LIST.exec(path)?.[1] ?? "";
        const stored = record(record(fresh.tree)["api-keys"])[family];
        const groups: unknown[] = Array.isArray(stored) ? stored.map((item: unknown) => item) : [];
        const existing = record(formValues(fresh.tree, path))[`${family}-api-key`];
        const entries: unknown[] = Array.isArray(existing) ? existing : [];
        if (request.method === "DELETE") {
          const index = Number(request.query?.index);
          const entry = record(entries[index]);
          const intended = request.configTarget;
          if (!Number.isInteger(index) || intended === undefined || entry["api-key"] !== intended.key ||
              (entry["base-url"] ?? "") !== (intended.baseUrl ?? "")) {
            throw new ConfigConflict([fieldName(`api-keys/${family}`)]);
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
              entries.some((entry, index) => !same(writable(entry), writable(next[index])))) {
            throw new ConfigConflict([fieldName(`api-keys/${family}`)]);
          }
          const key = writable(next.at(-1));
          const baseUrl = key["base-url"];
          delete key["base-url"];
          groups.push({ ...(baseUrl === undefined ? {} : { "base-url": baseUrl }), keys: [key] });
        }
        target = `${V8_CONFIG}/api-keys/${family}`;
        value = groups;
        change = { method: "PUT", json: value };
      }
    }
    return this.send<T>(target, change, fresh, path === CONFIG_YAML ? [] : [{ path: locations[0] ?? "", value }]);
  };
}
