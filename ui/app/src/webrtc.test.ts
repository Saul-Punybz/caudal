import { describe, expect, it, vi } from 'vitest';
import { WebRtcError, measureJitterBufferMs, startWhep, startWhip } from './webrtc';

// The test environment is plain Node (see vitest.config.ts) — no jsdom, and
// jsdom itself doesn't implement WebRTC anyway. `webrtc.ts` only needs a
// constructible `MediaStream` to collect incoming tracks into, so stub the
// minimal shape it uses (`addTrack`) globally for this file.
class FakeMediaStream {
  private tracks: MediaStreamTrack[] = [];
  addTrack(track: MediaStreamTrack): void {
    this.tracks.push(track);
  }
  getTracks(): MediaStreamTrack[] {
    return this.tracks;
  }
}
vi.stubGlobal('MediaStream', FakeMediaStream);

/** A fake RTCPeerConnection that implements just enough of the interface
 * for webrtc.ts: transceivers, offer/answer, ICE gathering state, and
 * getStats(). No real networking or media — this is what "mocked
 * RTCPeerConnection" means for a client that only speaks HTTP + SDP text. */
class FakePeerConnection {
  localDescription: RTCSessionDescriptionInit | null = null;
  remoteDescription: RTCSessionDescriptionInit | null = null;
  iceGatheringState: RTCIceGatheringState = 'complete';
  ontrack: ((ev: { track: MediaStreamTrack }) => void) | null = null;
  closed = false;
  transceivers: Array<{ direction: string; setCodecPreferences: ReturnType<typeof vi.fn> }> = [];
  statsReport: Map<string, Record<string, unknown>> = new Map();

  private listeners = new Map<string, Set<() => void>>();

  addTransceiver(_trackOrKind: unknown, opts?: { direction?: string }) {
    const t = { direction: opts?.direction ?? 'sendrecv', setCodecPreferences: vi.fn() };
    this.transceivers.push(t);
    return t as unknown as RTCRtpTransceiver;
  }

  async createOffer(): Promise<RTCSessionDescriptionInit> {
    return { type: 'offer', sdp: 'v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n' };
  }

  async setLocalDescription(desc: RTCSessionDescriptionInit): Promise<void> {
    this.localDescription = desc;
  }

  async setRemoteDescription(desc: RTCSessionDescriptionInit): Promise<void> {
    this.remoteDescription = desc;
  }

  addEventListener(type: string, cb: () => void): void {
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type)?.add(cb);
  }

  removeEventListener(type: string, cb: () => void): void {
    this.listeners.get(type)?.delete(cb);
  }

  /** Test helper: simulate the browser reaching a new ICE gathering state. */
  emitIceGatheringState(state: RTCIceGatheringState): void {
    this.iceGatheringState = state;
    this.listeners.get('icegatheringstatechange')?.forEach((cb) => cb());
  }

  close(): void {
    this.closed = true;
  }

  async getStats(): Promise<RTCStatsReport> {
    return this.statsReport as unknown as RTCStatsReport;
  }
}

function fakePc(): FakePeerConnection {
  return new FakePeerConnection();
}

function jsonHeaders(location: string) {
  return {
    get: (name: string) => (name.toLowerCase() === 'location' ? location : null),
  } as unknown as Headers;
}

function okResponse(answerSdp: string, location: string): Response {
  return {
    status: 201,
    headers: jsonHeaders(location),
    text: async () => answerSdp,
  } as unknown as Response;
}

function statusResponse(status: number): Response {
  return {
    status,
    headers: jsonHeaders(''),
    text: async () => '',
  } as unknown as Response;
}

describe('startWhep', () => {
  it('happy path: POSTs the offer, applies the answer, resolves the session URL', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(okResponse('v=0\r\no=answer\r\n', '/whep/demo/sess-1'));

    const session = await startWhep({
      url: 'https://caudal.example/whep/demo',
      token: 'abc123',
      pcFactory: () => pc as unknown as RTCPeerConnection,
      fetchImpl,
    });

    expect(fetchImpl).toHaveBeenCalledTimes(1);
    const [calledUrl, init] = fetchImpl.mock.calls[0];
    expect(calledUrl).toBe('https://caudal.example/whep/demo');
    expect(init.method).toBe('POST');
    expect(init.headers).toMatchObject({
      'content-type': 'application/sdp',
      authorization: 'Bearer abc123',
    });
    expect(init.body).toContain('v=0');

    expect(pc.remoteDescription).toEqual({ type: 'answer', sdp: 'v=0\r\no=answer\r\n' });
    expect(session.sessionUrl).toBe('https://caudal.example/whep/demo/sess-1');
    expect(session.stream).toBeInstanceOf(MediaStream);

    // recvonly video + audio transceivers, per the WHEP brief.
    expect(pc.transceivers).toHaveLength(2);
    expect(pc.transceivers.map((t) => t.direction)).toEqual(['recvonly', 'recvonly']);
  });

  it('401 -> WebRtcError with status 401 ("token needed")', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(statusResponse(401));

    await expect(
      startWhep({ url: 'https://caudal.example/whep/demo', pcFactory: () => pc as unknown as RTCPeerConnection, fetchImpl }),
    ).rejects.toMatchObject({ name: 'WebRtcError', status: 401 });
  });

  it('403 -> WebRtcError with status 403 ("token refused")', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(statusResponse(403));

    await expect(
      startWhep({ url: 'https://caudal.example/whep/demo', pcFactory: () => pc as unknown as RTCPeerConnection, fetchImpl }),
    ).rejects.toBeInstanceOf(WebRtcError);
  });

  it('rejects when the 201 response has no Location header', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue({
      status: 201,
      headers: jsonHeaders(''),
      text: async () => 'v=0\r\n',
    } as unknown as Response);

    await expect(
      startWhep({ url: 'https://caudal.example/whep/demo', pcFactory: () => pc as unknown as RTCPeerConnection, fetchImpl }),
    ).rejects.toThrow(/Location/);
  });

  it('resolves a relative Location against the request URL', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(okResponse('v=0\r\n', '/whep/demo/sess-xyz'));

    const session = await startWhep({
      url: 'https://caudal.example/whep/demo',
      pcFactory: () => pc as unknown as RTCPeerConnection,
      fetchImpl,
    });

    expect(session.sessionUrl).toBe('https://caudal.example/whep/demo/sess-xyz');
  });

  it('stop() DELETEs the session URL and closes the peer connection', async () => {
    const pc = fakePc();
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(okResponse('v=0\r\n', '/whep/demo/sess-1'))
      .mockResolvedValueOnce({ status: 200 } as Response);

    const session = await startWhep({
      url: 'https://caudal.example/whep/demo',
      pcFactory: () => pc as unknown as RTCPeerConnection,
      fetchImpl,
    });
    await session.stop();

    expect(fetchImpl).toHaveBeenCalledTimes(2);
    const [deleteUrl, deleteInit] = fetchImpl.mock.calls[1];
    expect(deleteUrl).toBe('https://caudal.example/whep/demo/sess-1');
    expect(deleteInit.method).toBe('DELETE');
    expect(pc.closed).toBe(true);
  });

  it('does not send an Authorization header when no token is given', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(okResponse('v=0\r\n', '/whep/demo/sess-1'));

    await startWhep({ url: 'https://caudal.example/whep/demo', pcFactory: () => pc as unknown as RTCPeerConnection, fetchImpl });

    const [, init] = fetchImpl.mock.calls[0];
    expect(init.headers.authorization).toBeUndefined();
  });

  it('caps ICE gathering wait instead of hanging forever', async () => {
    vi.useFakeTimers();
    try {
      const pc = fakePc();
      pc.iceGatheringState = 'gathering'; // never reaches 'complete'
      const fetchImpl = vi.fn().mockResolvedValue(okResponse('v=0\r\n', '/whep/demo/sess-1'));

      const promise = startWhep({
        url: 'https://caudal.example/whep/demo',
        iceGatheringTimeoutMs: 2000,
        pcFactory: () => pc as unknown as RTCPeerConnection,
        fetchImpl,
      });

      await vi.advanceTimersByTimeAsync(2000);
      const session = await promise;
      expect(session.sessionUrl).toBe('https://caudal.example/whep/demo/sess-1');
    } finally {
      vi.useRealTimers();
    }
  });
});

describe('startWhip', () => {
  function fakeVideoTrack(): MediaStreamTrack {
    return { kind: 'video', id: 'v1' } as unknown as MediaStreamTrack;
  }
  function fakeAudioTrack(): MediaStreamTrack {
    return { kind: 'audio', id: 'a1' } as unknown as MediaStreamTrack;
  }
  function fakeStream(tracks: MediaStreamTrack[]): MediaStream {
    return {
      getVideoTracks: () => tracks.filter((t) => t.kind === 'video'),
      getAudioTracks: () => tracks.filter((t) => t.kind === 'audio'),
    } as unknown as MediaStream;
  }

  it('happy path: sendonly transceivers, POSTs the offer, applies the answer', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(okResponse('v=0\r\no=answer\r\n', '/whip/demo/sess-1'));

    const session = await startWhip({
      url: 'https://caudal.example/whip/demo',
      stream: fakeStream([fakeVideoTrack(), fakeAudioTrack()]),
      pcFactory: () => pc as unknown as RTCPeerConnection,
      fetchImpl,
    });

    expect(pc.transceivers.map((t) => t.direction)).toEqual(['sendonly', 'sendonly']);
    expect(session.sessionUrl).toBe('https://caudal.example/whip/demo/sess-1');
    expect(pc.remoteDescription).toEqual({ type: 'answer', sdp: 'v=0\r\no=answer\r\n' });
  });

  it('prefers H.264 on the video transceiver when setCodecPreferences is available', async () => {
    const h264 = { mimeType: 'video/H264' };
    const vp8 = { mimeType: 'video/VP8' };
    vi.stubGlobal('RTCRtpSender', {
      getCapabilities: (kind: string) => (kind === 'video' ? { codecs: [vp8, h264] } : null),
    });
    try {
      const pc = fakePc();
      const fetchImpl = vi.fn().mockResolvedValue(okResponse('v=0\r\n', '/whip/demo/sess-1'));

      await startWhip({
        url: 'https://caudal.example/whip/demo',
        stream: fakeStream([fakeVideoTrack()]),
        pcFactory: () => pc as unknown as RTCPeerConnection,
        fetchImpl,
      });

      const videoTransceiver = pc.transceivers[0];
      expect(videoTransceiver.setCodecPreferences).toHaveBeenCalledWith([h264, vp8]);
    } finally {
      vi.unstubAllGlobals();
    }
  });

  it('401 -> WebRtcError with status 401', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(statusResponse(401));

    await expect(
      startWhip({
        url: 'https://caudal.example/whip/demo',
        stream: fakeStream([fakeVideoTrack()]),
        pcFactory: () => pc as unknown as RTCPeerConnection,
        fetchImpl,
      }),
    ).rejects.toMatchObject({ status: 401 });
  });

  it('403 -> WebRtcError with status 403', async () => {
    const pc = fakePc();
    const fetchImpl = vi.fn().mockResolvedValue(statusResponse(403));

    await expect(
      startWhip({
        url: 'https://caudal.example/whip/demo',
        stream: fakeStream([fakeVideoTrack()]),
        pcFactory: () => pc as unknown as RTCPeerConnection,
        fetchImpl,
      }),
    ).rejects.toMatchObject({ status: 403 });
  });

  it('stop() DELETEs the session and closes the peer connection', async () => {
    const pc = fakePc();
    const fetchImpl = vi
      .fn()
      .mockResolvedValueOnce(okResponse('v=0\r\n', '/whip/demo/sess-9'))
      .mockResolvedValueOnce({ status: 200 } as Response);

    const session = await startWhip({
      url: 'https://caudal.example/whip/demo',
      stream: fakeStream([fakeVideoTrack()]),
      pcFactory: () => pc as unknown as RTCPeerConnection,
      fetchImpl,
    });
    await session.stop();

    const [deleteUrl, deleteInit] = fetchImpl.mock.calls[1];
    expect(deleteUrl).toBe('https://caudal.example/whip/demo/sess-9');
    expect(deleteInit.method).toBe('DELETE');
    expect(pc.closed).toBe(true);
  });
});

describe('measureJitterBufferMs', () => {
  it('computes ms from jitterBufferDelay / jitterBufferEmittedCount', async () => {
    const pc = fakePc();
    pc.statsReport.set('inbound-rtp-video', {
      type: 'inbound-rtp',
      kind: 'video',
      jitterBufferDelay: 0.6,
      jitterBufferEmittedCount: 300,
    });
    const ms = await measureJitterBufferMs(pc as unknown as RTCPeerConnection);
    expect(ms).toBeCloseTo(2, 5); // 0.6 / 300 * 1000 = 2ms
  });

  it('returns null when no inbound-rtp report has emitted frames yet', async () => {
    const pc = fakePc();
    pc.statsReport.set('inbound-rtp-video', {
      type: 'inbound-rtp',
      kind: 'video',
      jitterBufferDelay: 0,
      jitterBufferEmittedCount: 0,
    });
    const ms = await measureJitterBufferMs(pc as unknown as RTCPeerConnection);
    expect(ms).toBeNull();
  });

  it('prefers the video row over audio when both are present', async () => {
    const pc = fakePc();
    pc.statsReport.set('inbound-rtp-audio', {
      type: 'inbound-rtp',
      kind: 'audio',
      jitterBufferDelay: 1,
      jitterBufferEmittedCount: 100,
    });
    pc.statsReport.set('inbound-rtp-video', {
      type: 'inbound-rtp',
      kind: 'video',
      jitterBufferDelay: 0.4,
      jitterBufferEmittedCount: 200,
    });
    const ms = await measureJitterBufferMs(pc as unknown as RTCPeerConnection);
    expect(ms).toBeCloseTo(2, 5); // the video row's 0.4/200*1000, not audio's 10
  });
});
