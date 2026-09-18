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
  ) {
    super(message);
    this.name = 'ApiError';
  }
}

async function getJson<T>(path: string): Promise<T> {
  let res: Response;
  try {
    res = await fetch(path, { headers: { accept: 'application/json' } });
  } catch (err) {
    throw new ApiError(`network error reaching ${path}: ${(err as Error).message}`);
  }
  if (!res.ok) {
    throw new ApiError(`${path} returned ${res.status}`, res.status);
  }
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
