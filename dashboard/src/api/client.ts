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

function isAbort(error: unknown): boolean {
  return error instanceof DOMException && error.name === "AbortError";
}

/**
 * Sends one request with `key`, as every call goes: no cookies, no cache,
 * no redirects, no referrer. Throws an ApiError, with the answer's `error`
 * and `message`, for any status from 400 up.
 */
async function send(
  key: string,
  url: string,
  accept: string,
  init: Pick<RequestInit, "method" | "body" | "signal"> & { contentType?: string | undefined },
): Promise<Response> {
  const headers: Record<string, string> = { Accept: accept, Authorization: `Bearer ${key}` };
  if (init.contentType !== undefined) {
    headers["Content-Type"] = init.contentType;
  }
  let response: Response;
  try {
    response = await globalThis.fetch(url, {
      method: init.method ?? "GET",
      headers,
      body: init.body,
      signal: init.signal,
      credentials: "omit",
      cache: "no-store",
      redirect: "error",
      referrerPolicy: "no-referrer",
    });
  } catch (error) {
    if (isAbort(error)) {
      throw error;
    }
    throw new ApiError(0, null, null, null);
  }
  if (response.status >= 400) {
    const data = await readBody(response);
    throw new ApiError(
      response.status,
      stringField(data, "error"),
      stringField(data, "message"),
      data,
    );
  }
  return response;
}

/** Calls the API with `key`. Throws an ApiError for any status from 400 up. */
export async function apiRequest<T = unknown>(
  key: string,
  path: string,
  request: ApiRequest = {},
): Promise<ApiResponse<T>> {
  let body: BodyInit | undefined = request.body;
  let contentType = request.contentType;
  if (request.json !== undefined) {
    contentType = "application/json";
    body = JSON.stringify(request.json);
  }
  const response = await send(key, apiUrl(path, request.query), "application/json", {
    method: request.method ?? "GET",
    body,
    signal: request.signal,
    contentType,
  });
  const data = await readBody(response);
  return { status: response.status, headers: response.headers, data: data as T };
}

/** How far a download has got. */
export interface DownloadProgress {
  received: number;
  /** The whole size, from Content-Length, when the server gave it. */
  total: number | null;
}

/**
 * A download that ended short of the size the server announced, as one
 * does when the file gets shorter while it is sent. Nothing is saved.
 */
export class ShortDownloadError extends Error {
  readonly received: number;
  readonly expected: number | null;

  constructor(received: number, expected: number | null) {
    super(
      expected === null
        ? `the download broke off after ${String(received)} bytes`
        : `the download ended after ${String(received)} of ${String(expected)} bytes`,
    );
    this.name = "ShortDownloadError";
    this.received = received;
    this.expected = expected;
  }
}

export function isShortDownload(error: unknown): error is ShortDownloadError {
  return error instanceof ShortDownloadError;
}

/** The size a response announces for its body, when it is the body's own. */
function announcedSize(headers: Headers): number | null {
  const length = headers.get("content-length");
  // A compressed body's Content-Length counts the compressed bytes.
  const encoding = headers.get("content-encoding") ?? "identity";
  if (length === null || !/^\d+$/.test(length) || encoding.toLowerCase() !== "identity") {
    return null;
  }
  return Number(length);
}

export interface DownloadRequest {
  signal?: AbortSignal;
  onProgress?: (progress: DownloadProgress) => void;
}

/**
 * Downloads a file the API sends whole, such as a log, with `key`: the
 * key goes in the Authorization header, never the URL. Errors are the
 * API's usual JSON, thrown as ApiError. A body that breaks off, or ends
 * short of its Content-Length, throws ShortDownloadError: a partial file
 * is never handed back.
 */
export async function apiDownload(
  key: string,
  path: string,
  { signal, onProgress }: DownloadRequest = {},
): Promise<Blob> {
  const response = await send(key, apiUrl(path), "application/octet-stream, application/json", {
    signal,
  });
  const total = announcedSize(response.headers);
  const chunks: Uint8Array[] = [];
  let received = 0;
  onProgress?.({ received, total });
  if (response.body !== null) {
    const reader = response.body.getReader();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) {
          break;
        }
        chunks.push(value);
        received += value.byteLength;
        onProgress?.({ received, total });
      }
    } catch (error) {
      if (isAbort(error) || signal?.aborted === true) {
        throw error;
      }
      throw new ShortDownloadError(received, total);
    }
  }
  if (total !== null && received !== total) {
    throw new ShortDownloadError(received, total);
  }
  return new Blob(chunks as BlobPart[], { type: "application/octet-stream" });
}
