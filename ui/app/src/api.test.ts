import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  ApiError,
  getSession,
  getStream,
  login,
  logout,
  setCsrfToken,
  setLoginRequiredHandler,
  listChannels,
  listRecordings,
  listRestreams,
  listStreams,
  requestClip,
  skipChannel,
  vodPlaylistUrl,
  thumbnailUrl,
  listSessions,
  kickSession,
  listPulls,
  listPushes,
  listAutoPushes,
  type ChannelStatus,
  type Recording,
  type RestreamStatus,
  type Stream,
} from './api';

const sample: Stream = {
  name: 'test',
  tracks: [
    {
      id: 0,
      kind: 'video',
      codec: 'h264',
      timescale: 90000,
      width: 1280,
      height: 720,
      fps: null,
      sample_rate: null,
      channels: null,
      lang: null,
    },
  ],
  stats: {
    frames_in: 0,
    bytes_in: 0,
    frames_buffered: 0,
    bytes_buffered: 0,
    buffered_ms: 0,
    viewers: 0,
  },
};

afterEach(() => {
  vi.unstubAllGlobals();
  setCsrfToken(null);
  setLoginRequiredHandler(null);
});

describe('listStreams', () => {
  it('fetches and parses the streams array', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => [sample],
    });
    vi.stubGlobal('fetch', fetchMock);

    const streams = await listStreams();
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/v1/streams',
      expect.objectContaining({ headers: expect.any(Object) }),
    );
    expect(streams).toEqual([sample]);
  });

  it('throws ApiError on a non-ok response', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: false, status: 503, json: async () => ({}) }),
    );
    await expect(listStreams()).rejects.toBeInstanceOf(ApiError);
  });

  it('throws ApiError when the network request itself fails', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn().mockRejectedValue(new Error('connection refused')),
    );
    await expect(listStreams()).rejects.toBeInstanceOf(ApiError);
  });
});

describe('getStream', () => {
  it('fetches a single stream by name, URL-encoded', async () => {
    const fetchMock = vi.fn().mockResolvedValue({
      ok: true,
      status: 200,
      json: async () => sample,
    });
    vi.stubGlobal('fetch', fetchMock);

    const stream = await getStream('my stream');
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/v1/streams/my%20stream',
      expect.anything(),
    );
    expect(stream).toEqual(sample);
  });
});

const channel: ChannelStatus = {
  name: 'channel-1',
  state: 'playing',
  index: 2,
  items: 5,
  now_playing: { path: '/media/a.mp4', position_secs: 12.5, duration_secs: 60 },
  error: null,
};

const restream: RestreamStatus = {
  stream: 'main',
  target: 'rtmp://example.com/live/****',
  state: 'live',
  bytes_sent: 1024,
  since_secs: 30,
  last_error: null,
};

const recording: Recording = {
  stream: 'main',
  id: '20260918T101500Z',
  started_at: '2026-09-18T10:15:00.000Z',
  ended_at: null,
  duration_ms: 60000,
  bytes: 2048,
  segments: 6,
  tracks: [{ kind: 'video', codec: 'h264', width: 1280, height: 720, sample_rate: null }],
};

function jsonResponse(status: number, body: unknown, ok = status >= 200 && status < 300) {
  return {
    ok,
    status,
    headers: new Headers({ 'content-type': 'application/json' }),
    json: async () => body,
  };
}

function htmlFallbackResponse() {
  return {
    ok: true,
    status: 200,
    headers: new Headers({ 'content-type': 'text/html; charset=utf-8' }),
    json: async () => {
      throw new Error('not JSON');
    },
  };
}

describe('listChannels', () => {
  it('fetches and parses the channels array', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, [channel])));
    await expect(listChannels()).resolves.toEqual([channel]);
  });

  it('resolves to an empty array on a 404 (route not mounted)', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(404, 'not found', false)));
    await expect(listChannels()).resolves.toEqual([]);
  });

  it('resolves to an empty array when the SPA fallback serves index.html', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(htmlFallbackResponse()));
    await expect(listChannels()).resolves.toEqual([]);
  });

  it('throws ApiError on a real server error', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(500, 'boom', false)));
    await expect(listChannels()).rejects.toBeInstanceOf(ApiError);
  });

  it('throws ApiError when the network request itself fails', async () => {
    vi.stubGlobal('fetch', vi.fn().mockRejectedValue(new Error('connection refused')));
    await expect(listChannels()).rejects.toBeInstanceOf(ApiError);
  });
});

describe('skipChannel', () => {
  it('posts to the skip endpoint with an optional bearer token', async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, status: 204 });
    vi.stubGlobal('fetch', fetchMock);

    await skipChannel('channel 1', 'secret');
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/v1/channels/channel%201/skip',
      expect.objectContaining({ method: 'POST', headers: { authorization: 'Bearer secret' } }),
    );
  });

  it('throws ApiError with the status on 404 (unknown channel)', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 404 }));
    await expect(skipChannel('nope')).rejects.toMatchObject({ status: 404 });
  });

  it('throws ApiError with the status on 401 (token required)', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 401 }));
    await expect(skipChannel('main')).rejects.toMatchObject({ status: 401 });
  });
});

describe('listRestreams', () => {
  it('fetches and parses the restreams array', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, [restream])));
    await expect(listRestreams()).resolves.toEqual([restream]);
  });

  it('resolves to an empty array when no targets are configured', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(htmlFallbackResponse()));
    await expect(listRestreams()).resolves.toEqual([]);
  });
});

describe('listRecordings', () => {
  it('fetches and parses the recordings array', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, [recording])));
    await expect(listRecordings()).resolves.toEqual([recording]);
  });

  it('resolves to an empty array when recording is disabled', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(htmlFallbackResponse()));
    await expect(listRecordings()).resolves.toEqual([]);
  });

  it('resolves to a real empty array when recording is enabled but empty', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, [])));
    await expect(listRecordings()).resolves.toEqual([]);
  });
});

describe('requestClip', () => {
  it('posts the clip range and returns the response body as a blob', async () => {
    const blob = new Blob(['mp4 bytes']);
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, status: 200, blob: async () => blob });
    vi.stubGlobal('fetch', fetchMock);

    const result = await requestClip({ stream: 'main', id: '20260918T101500Z', fromMs: 0, toMs: 60000 });
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/v1/clips',
      expect.objectContaining({
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ stream: 'main', id: '20260918T101500Z', from_ms: 0, to_ms: 60000 }),
      }),
    );
    expect(result).toBe(blob);
  });

  it('throws ApiError on a non-ok response', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 404 }));
    await expect(requestClip({ stream: 'main', id: 'x', fromMs: 0, toMs: 1 })).rejects.toBeInstanceOf(ApiError);
  });
});

describe('vodPlaylistUrl', () => {
  it('builds the VOD playlist path, URL-encoded', () => {
    expect(vodPlaylistUrl('my stream', '20260918T101500Z')).toBe('/vod/my%20stream/20260918T101500Z/index.m3u8');
  });
});

describe('admin login', () => {
  const session = {
    required: true,
    authenticated: true,
    user: 'ana',
    csrf_token: 'tok123',
    password: true,
    oidc: false,
  };

  it('getSession stores the CSRF token and later writes send it', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, session));
    vi.stubGlobal('fetch', fetchMock);
    await expect(getSession()).resolves.toEqual(session);

    fetchMock.mockResolvedValue({ ok: true, status: 204 });
    await skipChannel('main');
    expect(fetchMock).toHaveBeenLastCalledWith(
      '/api/v1/channels/main/skip',
      expect.objectContaining({ method: 'POST', headers: { 'x-csrf-token': 'tok123' } }),
    );
  });

  it('never sends the CSRF token on reads', async () => {
    setCsrfToken('tok123');
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, []));
    vi.stubGlobal('fetch', fetchMock);
    await listStreams();
    expect(fetchMock.mock.calls[0][1].headers).not.toHaveProperty('x-csrf-token');
  });

  it("a gate 401 calls the login handler; a route's own token 401 does not", async () => {
    const handler = vi.fn();
    setLoginRequiredHandler(handler);
    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: false, status: 401, headers: new Headers({ 'x-caudal-login': 'required' }) }),
    );
    await expect(listStreams()).rejects.toMatchObject({ status: 401 });
    expect(handler).toHaveBeenCalledTimes(1);

    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: false, status: 401, headers: new Headers({ 'www-authenticate': 'Bearer' }) }),
    );
    await expect(skipChannel('main')).rejects.toMatchObject({ status: 401 });
    expect(handler).toHaveBeenCalledTimes(1);
  });

  it('login posts JSON, keeps the CSRF token, and reports rate limits', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, { user: 'ana', csrf_token: 'fresh' }));
    vi.stubGlobal('fetch', fetchMock);
    await expect(login('ana', 'pw')).resolves.toEqual({ user: 'ana' });
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/v1/auth/login',
      expect.objectContaining({ method: 'POST', body: JSON.stringify({ name: 'ana', password: 'pw' }) }),
    );

    fetchMock.mockResolvedValue({ ok: true, status: 204 });
    await logout();
    expect(fetchMock).toHaveBeenLastCalledWith(
      '/api/v1/auth/logout',
      expect.objectContaining({ headers: { 'x-csrf-token': 'fresh' } }),
    );

    vi.stubGlobal(
      'fetch',
      vi.fn().mockResolvedValue({ ok: false, status: 429, headers: new Headers({ 'retry-after': '42' }) }),
    );
    await expect(login('ana', 'pw')).rejects.toMatchObject({ status: 429, retryAfter: 42 });
  });

  it('a server without login routes reads as "not required"', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(htmlFallbackResponse()));
    await expect(getSession()).resolves.toMatchObject({ required: false });
  });
});

describe('thumbnailUrl', () => {
  it('encodes the stream name and adds the width and a cache buster', () => {
    expect(thumbnailUrl('cam 1', 640)).toBe('/api/v1/streams/cam%201/thumbnail.jpg?width=640');
    expect(thumbnailUrl('cam', 320, 42)).toBe('/api/v1/streams/cam/thumbnail.jpg?width=320&t=42');
  });
});

describe('viewer sessions', () => {
  const session = {
    id: 17,
    stream: 'cam',
    protocol: 'srt',
    ip: '203.0.113.9',
    started: 1763586245,
    duration_secs: 312,
    bytes_out: 48211904,
    rtt_ms: 18.4,
    loss: 0.003,
    tags: ['vip'],
  };

  it('lists one stream\'s sessions', async () => {
    const fetchMock = vi.fn().mockResolvedValue(jsonResponse(200, [session]));
    vi.stubGlobal('fetch', fetchMock);
    await expect(listSessions('my cam')).resolves.toEqual([session]);
    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/sessions?stream=my%20cam');
  });

  it('resolves to an empty array on an older server without the route', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(htmlFallbackResponse()));
    await expect(listSessions()).resolves.toEqual([]);
  });

  it('kicks with DELETE and the CSRF token', async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: true, status: 200 });
    vi.stubGlobal('fetch', fetchMock);
    setCsrfToken('csrf-1');
    await kickSession(17);
    expect(fetchMock).toHaveBeenCalledWith(
      '/api/v1/sessions/17',
      expect.objectContaining({ method: 'DELETE', headers: { 'x-csrf-token': 'csrf-1' } }),
    );
  });

  it('passes block_secs and throws with the status on 404', async () => {
    const fetchMock = vi.fn().mockResolvedValue({ ok: false, status: 404 });
    vi.stubGlobal('fetch', fetchMock);
    await expect(kickSession(9, 60)).rejects.toMatchObject({ status: 404 });
    expect(fetchMock.mock.calls[0][0]).toBe('/api/v1/sessions/9?block_secs=60');
  });
});

describe('pulls and pushes', () => {
  const entry = { stream: 'cam1', url: 'rtsp://10.0.0.5/live', section: '[[rtsp.pull]]' };
  const rule = {
    streams: ['cam*'],
    url: 'srt://203.0.113.4:9000?streamid=****',
    kind: 'srt',
    active: [{ stream: 'cam1', url: 'srt://203.0.113.4:9000?streamid=****' }],
    skipped: [],
  };

  it('parses each listing', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, [entry])));
    await expect(listPulls()).resolves.toEqual([entry]);
    await expect(listPushes()).resolves.toEqual([entry]);
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(200, [rule])));
    await expect(listAutoPushes()).resolves.toEqual([rule]);
  });

  it('resolves to empty arrays when the config routes are not mounted', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(jsonResponse(404, 'not found', false)));
    await expect(listPulls()).resolves.toEqual([]);
    await expect(listPushes()).resolves.toEqual([]);
    await expect(listAutoPushes()).resolves.toEqual([]);
  });
});
