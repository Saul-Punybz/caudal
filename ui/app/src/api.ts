// Typed client for the Caudal HTTP API.
//
// The JSON shape below is fixed by STATUS.md / the batch brief. Do not
// invent fields the server does not send, and do not guess at fields it
// might send later (e.g. uptime, egress, an events feed) — the UI must
// only show what this API actually provides.

export type TrackKind = 'video' | 'audio' | string;

export interface Track {
  id: number;
  kind: TrackKind;
  codec: string;
  timescale: number;
  width: number | null;
  height: number | null;
  fps: number | null;
  sample_rate: number | null;
  channels: number | null;
  lang: string | null;
}

export interface StreamStats {
  frames_in: number;
  bytes_in: number;
  frames_buffered: number;
  bytes_buffered: number;
  buffered_ms: number;
  viewers: number;
}

export interface Stream {
  name: string;
  tracks: Track[];
  stats: StreamStats;
}

export class ApiError extends Error {
  constructor(
    message: string,
    public status?: number,
    /** Seconds, from `Retry-After` on a 429. */
    public retryAfter?: number,
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

// ---- Admin login (crates/caudal-admin) ----

/** The CSRF token of the current admin session. Sent as `x-csrf-token` on
 * every state-changing request; `null` when login is off or signed out. */
let csrfToken: string | null = null;
let onLoginRequired: (() => void) | null = null;

export function setCsrfToken(token: string | null): void {
  csrfToken = token;
}

/** Called when the server's admin gate answers 401 (session missing or
 * expired). The gate marks those 401s with `x-caudal-login`, so a stream
 * token refused by a route's own check (channel skip) is not mistaken for
 * a lost login. */
export function setLoginRequiredHandler(handler: (() => void) | null): void {
  onLoginRequired = handler;
}

function isSafeMethod(method: string | undefined): boolean {
  const m = (method ?? 'GET').toUpperCase();
  return m === 'GET' || m === 'HEAD' || m === 'OPTIONS';
}

async function request(path: string, init?: RequestInit): Promise<Response> {
  let finalInit = init;
  if (csrfToken && !isSafeMethod(init?.method)) {
    finalInit = { ...init, headers: { ...(init?.headers as Record<string, string>), 'x-csrf-token': csrfToken } };
  }
  let res: Response;
  try {
    res = await fetch(path, finalInit);
  } catch (err) {
    throw new ApiError(`network error reaching ${path}: ${(err as Error).message}`);
  }
  if (res.status === 401 && res.headers?.get?.('x-caudal-login') && onLoginRequired) {
    onLoginRequired();
  }
  return res;
}

export interface AuthSession {
  /** Whether this server requires an admin login at all. */
  required: boolean;
  authenticated: boolean;
  user: string | null;
  csrf_token: string | null;
  /** Local name + password login is configured. */
  password: boolean;
  /** "Sign in with SSO" (OIDC) is configured. */
  oidc: boolean;
}

/** GET /api/v1/auth/session — always public. */
export async function getSession(): Promise<AuthSession> {
  const res = await request('/api/v1/auth/session', { headers: { accept: 'application/json' } });
  if (!res.ok) throw new ApiError(`/api/v1/auth/session returned ${res.status}`, res.status);
  const contentType = res.headers.get('content-type') ?? '';
  // An older server without the auth routes serves the SPA here.
  if (!contentType.includes('application/json')) {
    return { required: false, authenticated: false, user: null, csrf_token: null, password: false, oidc: false };
  }
  const session = (await res.json()) as AuthSession;
  setCsrfToken(session.csrf_token);
  return session;
}

/** POST /api/v1/auth/login. Throws ApiError with status 401 (wrong name
 * or password) or 429 (rate limited; `retryAfter` in seconds). */
export async function login(name: string, password: string): Promise<{ user: string }> {
  const path = '/api/v1/auth/login';
  let res: Response;
  try {
    res = await fetch(path, {
      method: 'POST',
      headers: { 'content-type': 'application/json', accept: 'application/json' },
      body: JSON.stringify({ name, password }),
    });
  } catch (err) {
    throw new ApiError(`network error reaching ${path}: ${(err as Error).message}`);
  }
  if (!res.ok) {
    const retryAfter = Number(res.headers.get('retry-after')) || undefined;
    throw new ApiError(`${path} returned ${res.status}`, res.status, retryAfter);
  }
  const body = (await res.json()) as { user: string; csrf_token: string };
  setCsrfToken(body.csrf_token);
  return { user: body.user };
}

/** POST /api/v1/auth/logout */
export async function logout(): Promise<void> {
  await request('/api/v1/auth/logout', { method: 'POST' });
  setCsrfToken(null);
}

/** Where "Sign in with SSO" sends the browser (a full page navigation). */
export const SSO_START_URL = '/api/v1/auth/oidc/start';

async function getJson<T>(path: string): Promise<T> {
  const res = await request(path, { headers: { accept: 'application/json' } });
  if (!res.ok) {
    throw new ApiError(`${path} returned ${res.status}`, res.status);
  }
  return (await res.json()) as T;
}

/**
 * Like `getJson`, but for a route whose whole feature is optional
 * (channels, restreams, recordings). `crates/caudal/src/main.rs` only
 * merges that feature's router when it's configured/enabled; when it
 * isn't, the request falls through to `caudal-ui`'s SPA catch-all, which
 * serves `index.html` — 200, `text/html`, not JSON. A real 404 means the
 * same thing for routes that are always mounted. Both collapse to
 * `emptyValue` so the screen shows an empty state instead of a JSON parse
 * error or a scary error banner.
 */
async function getJsonFeature<T>(path: string, emptyValue: T): Promise<T> {
  const res = await request(path, { headers: { accept: 'application/json' } });
  if (res.status === 404) return emptyValue;
  if (!res.ok) {
    throw new ApiError(`${path} returned ${res.status}`, res.status);
  }
  const contentType = res.headers.get('content-type') ?? '';
  if (!contentType.includes('application/json')) return emptyValue;
  return (await res.json()) as T;
}

/** GET /api/v1/streams */
export function listStreams(): Promise<Stream[]> {
  return getJson<Stream[]>('/api/v1/streams');
}

/** GET /api/v1/streams/:name */
export function getStream(name: string): Promise<Stream> {
  return getJson<Stream>(`/api/v1/streams/${encodeURIComponent(name)}`);
}

export interface MoqFingerprint {
  /** The relay's own base URL, e.g. `https://host:port`. */
  url: string;
  /** Lowercase hex SHA-256 of the relay's certificate, or null when the
   * server has a real (CA-signed) certificate the browser already trusts. */
  fingerprint: string | null;
}

/** GET /moq/fingerprint */
export function getMoqFingerprint(): Promise<MoqFingerprint> {
  return getJson<MoqFingerprint>('/moq/fingerprint');
}

// ---- 24/7 channels (crates/caudal-channel/src/http.rs) ----

export type ChannelState = 'playing' | 'idle' | 'error';

export interface NowPlaying {
  path: string;
  position_secs: number;
  /** `null` when the container doesn't say (estimated for TS). */
  duration_secs: number | null;
}

export interface ChannelStatus {
  name: string;
  state: ChannelState;
  /** Position of the current (or last) item in this pass's playlist. */
  index: number;
  /** Number of items in this pass's playlist, directories expanded. */
  items: number;
  now_playing: NowPlaying | null;
  /** The most recent error, cleared after a pass with no errors. */
  error: string | null;
}

/** GET /api/v1/channels. Empty array when no 24/7 channels are configured. */
export function listChannels(): Promise<ChannelStatus[]> {
  return getJsonFeature<ChannelStatus[]>('/api/v1/channels', []);
}

/** POST /api/v1/channels/:name/skip — jumps the channel to its next item
 * at once. Needs Publish access on the channel's stream name. */
export async function skipChannel(name: string, token?: string): Promise<void> {
  const path = `/api/v1/channels/${encodeURIComponent(name)}/skip`;
  const headers: Record<string, string> = {};
  if (token) headers.authorization = `Bearer ${token}`;
  const res = await request(path, { method: 'POST', headers });
  if (!res.ok) {
    throw new ApiError(`${path} returned ${res.status}`, res.status);
  }
}

// ---- Restreams (crates/caudal-restream/src/http.rs) ----

export type RestreamState = 'waiting' | 'connecting' | 'live' | 'retrying';

export interface RestreamStatus {
  stream: string;
  /** `scheme://host/app/****`, already redacted by the server. */
  target: string;
  state: RestreamState;
  bytes_sent: number;
  since_secs: number;
  last_error: string | null;
}

/** GET /api/v1/restreams. Empty array when no restream targets are configured. */
export function listRestreams(): Promise<RestreamStatus[]> {
  return getJsonFeature<RestreamStatus[]>('/api/v1/restreams', []);
}

// ---- Recordings (crates/caudal-record/src/http.rs, src/meta.rs) ----

export interface RecordingTrack {
  kind: string;
  codec: string;
  width: number | null;
  height: number | null;
  sample_rate: number | null;
}

export interface Recording {
  stream: string;
  id: string;
  started_at: string;
  /** `null` while the recording is still in progress. */
  ended_at: string | null;
  duration_ms: number;
  bytes: number;
  segments: number;
  tracks: RecordingTrack[];
  /** Why the recording stopped early (disk full, crash) — absent otherwise. */
  error?: string;
}

/** GET /api/v1/recordings. Empty array when recording isn't enabled, or
 * nothing has been recorded yet (or the caller has no Play token for any
 * of it). */
export function listRecordings(): Promise<Recording[]> {
  return getJsonFeature<Recording[]>('/api/v1/recordings', []);
}

/** GET /api/v1/recordings/:stream/:id */
export function getRecording(stream: string, id: string): Promise<Recording> {
  return getJson<Recording>(`/api/v1/recordings/${encodeURIComponent(stream)}/${encodeURIComponent(id)}`);
}

/** `/vod/{stream}/{id}/index.m3u8` — the recording's HLS VOD playlist. */
export function vodPlaylistUrl(stream: string, id: string): string {
  const origin = typeof window !== 'undefined' ? window.location.origin : '';
  return `${origin}/vod/${encodeURIComponent(stream)}/${encodeURIComponent(id)}/index.m3u8`;
}

export interface ClipRequest {
  stream: string;
  id: string;
  fromMs: number;
  toMs: number;
}

/** POST /api/v1/clips — cuts `[fromMs, toMs)` out of a recording into a
 * standalone MP4 and streams it back. Used both for real clips and, with
 * `fromMs: 0, toMs: duration_ms`, to download a whole recording. */
export async function requestClip(req: ClipRequest): Promise<Blob> {
  const path = '/api/v1/clips';
  const res = await request(path, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ stream: req.stream, id: req.id, from_ms: req.fromMs, to_ms: req.toMs }),
  });
  if (!res.ok) {
    throw new ApiError(`${path} returned ${res.status}`, res.status);
  }
  return res.blob();
}
