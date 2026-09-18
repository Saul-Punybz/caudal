import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, getStream, listStreams, type Stream } from './api';

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
