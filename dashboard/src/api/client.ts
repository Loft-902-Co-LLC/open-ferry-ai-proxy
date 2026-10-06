// Calls to the server the app was loaded from: upstream's management API
// under /v0/management/ and /v8/management/, and open-ferry's dashboard API
// under /open-ferry/api/v1/. Nothing else is ever fetched.
//
// Every call carries the management key as `Authorization: Bearer <key>`,
// sends no cookies, follows no redirects and isn't cached.

export type HttpMethod = "GET" | "POST" | "PUT" | "PATCH" | "DELETE";

/** The path prefixes the app may call. */
export const API_PREFIXES = ["/v0/management/", "/v8/management/", "/open-ferry/api/v1/"] as const;

/** A request the server answered with an error, or didn't answer. */
export class ApiError extends Error {
  /** The HTTP status, or 0 when no answer came. */
  readonly status: number;
  /** The answer's `error` field, such as `invalid management key`. */
  readonly code: string | null;
  /** The answer's `message` field, when it has one. */
  readonly detail: string | null;
  /** The whole answer, parsed as JSON where it was JSON. */
  readonly body: unknown;

  constructor(status: number, code: string | null, detail: string | null, body: unknown) {
    super(
      status === 0
        ? "the server didn't answer"
        : `HTTP ${String(status)}${code === null ? "" : `: ${code}`}`,
    );
    this.name = "ApiError";
    this.status = status;
    this.code = code;
    this.detail = detail;
    this.body = body;
  }
}

export function isApiError(error: unknown): error is ApiError {
  return error instanceof ApiError;
}

/** Whether `error` is the server's empty 404 for a route it doesn't serve. */
export function isUnsupportedRoute(error: unknown): boolean {
  return isApiError(error) && error.status === 404 && error.code === null;
}

export interface ApiRequest {
  method?: HttpMethod;
  /** Query parameters; undefined values are left out. */
  query?: Record<string, string | number | boolean | undefined>;
  /** A body to send as JSON. */
  json?: unknown;
  /** A body to send as it is, such as a file or YAML text. */
  body?: BodyInit;
  /** The body's content type, for `body`. */
  contentType?: string;
  signal?: AbortSignal;
}

export interface ApiResponse<T = unknown> {
  status: number;
  headers: Headers;
  data: T;
}

/** The URL for `path` and `query`, refusing anything outside the API. */
export function apiUrl(path: string, query?: ApiRequest["query"]): string {
  if (!API_PREFIXES.some((prefix) => path.startsWith(prefix)) || path.includes("..")) {
    throw new Error(`not an API path: ${path}`);
  }
  const params = new URLSearchParams();
  for (const [name, value] of Object.entries(query ?? {})) {
    if (value !== undefined) {
      params.append(name, String(value));
    }
  }
  const search = params.toString();
  return search === "" ? path : `${path}?${search}`;
}

/** Reads an answer's body: JSON when it says so and parses, else text. */
async function readBody(response: Response): Promise<unknown> {
  const text = await response.text();
  const type = response.headers.get("content-type") ?? "";
  if (type.includes("json")) {
    if (text === "") {
      return null;
    }
    try {
      return JSON.parse(text) as unknown;
    } catch {
      return text;
    }
  }
  return text;
}

function stringField(body: unknown, name: string): string | null {
  if (body !== null && typeof body === "object" && name in body) {
    const value = (body as Record<string, unknown>)[name];
    return typeof value === "string" ? value : null;
  }
  return null;
}

/** Calls the API with `key`. Throws an ApiError for any status from 400 up. */
export async function apiRequest<T = unknown>(
  key: string,
  path: string,
  request: ApiRequest = {},
): Promise<ApiResponse<T>> {
  const headers: Record<string, string> = {
    Accept: "application/json",
    Authorization: `Bearer ${key}`,
  };
  let body: BodyInit | undefined = request.body;
  if (request.json !== undefined) {
    headers["Content-Type"] = "application/json";
    body = JSON.stringify(request.json);
  } else if (request.contentType !== undefined) {
    headers["Content-Type"] = request.contentType;
  }
  let response: Response;
  try {
    response = await globalThis.fetch(apiUrl(path, request.query), {
      method: request.method ?? "GET",
      headers,
      body,
      signal: request.signal,
      credentials: "omit",
      cache: "no-store",
      redirect: "error",
      referrerPolicy: "no-referrer",
    });
  } catch (error) {
    if (error instanceof DOMException && error.name === "AbortError") {
      throw error;
    }
    throw new ApiError(0, null, null, null);
  }
  const data = await readBody(response);
  if (response.status >= 400) {
    throw new ApiError(
      response.status,
      stringField(data, "error"),
      stringField(data, "message"),
      data,
    );
  }
  return { status: response.status, headers: response.headers, data: data as T };
}
