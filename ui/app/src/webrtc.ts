// Tiny WHEP (play) and WHIP (publish) clients over the browser's
// RTCPeerConnection. No signalling library: WHEP/WHIP is just "POST an SDP
// offer, get an SDP answer back", per the IETF drafts the server implements
// (see crates/caudal-webrtc). Deliberately dependency-free — the whole
// protocol fits in a couple of fetch calls, so pulling in a library (even a
// small MIT one) would cost more bytes than it saves.
//
// Server contract (fixed, STATUS.md "Batch 4"):
//   POST /whep/{name} or /whip/{name}, body = SDP offer,
//     Content-Type: application/sdp
//   -> 201 + SDP answer body + `Location: /whep/{name}/{session}` (or whip)
//   DELETE that Location to stop.
//   Auth: `Authorization: Bearer <token>` (or `?token=` on the URL).
//   401 = token needed, 403 = token refused.

/** Minimal fetch signature, so tests can inject a mock without touching the
 * global. */
export type FetchLike = (input: string, init?: RequestInit) => Promise<Response>;

export class WebRtcError extends Error {
  constructor(
    message: string,
    public status?: number,
  ) {
    super(message);
    this.name = 'WebRtcError';
  }
}

export interface WebRtcSession {
  pc: RTCPeerConnection;
  /** Absolute URL of the WHEP/WHIP session resource, from the `Location` header. */
  sessionUrl: string;
  /** DELETE the session, then close the local peer connection. */
  stop: () => Promise<void>;
}

export interface WhepSession extends WebRtcSession {
  /** Populated as tracks arrive over `ontrack`; attach to a <video> via `srcObject`. */
  stream: MediaStream;
}

const DEFAULT_ICE_GATHERING_TIMEOUT_MS = 2000;

/** Waits for ICE gathering to finish so the offer carries every host/srflx
 * candidate (no trickle ICE support on the wire), but never waits longer
 * than `timeoutMs` — some networks never reach "complete". */
function waitForIceGatheringComplete(pc: RTCPeerConnection, timeoutMs: number): Promise<void> {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    let settled = false;
    const finish = () => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      pc.removeEventListener('icegatheringstatechange', onChange);
      resolve();
    };
    const onChange = () => {
      if (pc.iceGatheringState === 'complete') finish();
    };
    const timer = setTimeout(finish, timeoutMs);
    pc.addEventListener('icegatheringstatechange', onChange);
  });
}

interface NegotiateOptions {
  pc: RTCPeerConnection;
  url: string;
  token?: string;
  iceGatheringTimeoutMs: number;
  fetchImpl: FetchLike;
}

/** Shared POST-offer / apply-answer flow for both WHEP and WHIP. */
async function negotiate({ pc, url, token, iceGatheringTimeoutMs, fetchImpl }: NegotiateOptions): Promise<string> {
  const offer = await pc.createOffer();
  await pc.setLocalDescription(offer);
  await waitForIceGatheringComplete(pc, iceGatheringTimeoutMs);

  const sdp = pc.localDescription?.sdp ?? offer.sdp;
  if (!sdp) throw new WebRtcError('failed to build a local SDP offer');

  const headers: Record<string, string> = { 'content-type': 'application/sdp' };
  if (token) headers.authorization = `Bearer ${token}`;

  const res = await fetchImpl(url, { method: 'POST', headers, body: sdp });

  if (res.status === 401) throw new WebRtcError('token needed', 401);
  if (res.status === 403) throw new WebRtcError('token refused', 403);
  if (res.status !== 201) throw new WebRtcError(`unexpected response: ${res.status}`, res.status);

  const answerSdp = await res.text();
  const location = res.headers.get('location') ?? res.headers.get('Location');
  if (!location) throw new WebRtcError('server did not send a Location header for the session');

  await pc.setRemoteDescription({ type: 'answer', sdp: answerSdp });

  return new URL(location, url).toString();
}

async function stopSession(pc: RTCPeerConnection, sessionUrl: string, fetchImpl: FetchLike): Promise<void> {
  try {
    await fetchImpl(sessionUrl, { method: 'DELETE' });
  } finally {
    pc.close();
  }
}

export interface WhepOptions {
  /** e.g. `${origin}/whep/${name}` */
  url: string;
  token?: string;
  iceGatheringTimeoutMs?: number;
  /** Overridable for tests; defaults to `new RTCPeerConnection()`. */
  pcFactory?: () => RTCPeerConnection;
  /** Overridable for tests; defaults to the global `fetch`. */
  fetchImpl?: FetchLike;
}

/** WHEP: play a stream. Recvonly video + audio, offer/answer, then tracks
 * arrive over `ontrack` into the returned `MediaStream`. */
export async function startWhep(opts: WhepOptions): Promise<WhepSession> {
  const pcFactory = opts.pcFactory ?? (() => new RTCPeerConnection());
  const fetchImpl = opts.fetchImpl ?? ((input: string, init?: RequestInit) => fetch(input, init));
  const pc = pcFactory();
  const stream = new MediaStream();

  pc.ontrack = (ev) => {
    stream.addTrack(ev.track);
  };
  pc.addTransceiver('video', { direction: 'recvonly' });
  pc.addTransceiver('audio', { direction: 'recvonly' });

  const sessionUrl = await negotiate({
    pc,
    url: opts.url,
    token: opts.token,
    iceGatheringTimeoutMs: opts.iceGatheringTimeoutMs ?? DEFAULT_ICE_GATHERING_TIMEOUT_MS,
    fetchImpl,
  });

  return { pc, stream, sessionUrl, stop: () => stopSession(pc, sessionUrl, fetchImpl) };
}

export interface WhipOptions {
  /** e.g. `${origin}/whip/${name}` */
  url: string;
  token?: string;
  /** From `getUserMedia({ video, audio })`. */
  stream: MediaStream;
  iceGatheringTimeoutMs?: number;
  pcFactory?: () => RTCPeerConnection;
  fetchImpl?: FetchLike;
}

/** Reorders a video transceiver's codec preferences so H.264 is tried
 * first — the server requires it. No-op where the browser doesn't support
 * `setCodecPreferences` (Safari < 17) or has no H.264 encoder at all;
 * `createOffer` then just offers whatever the browser supports. */
function preferH264(transceiver: RTCRtpTransceiver): void {
  if (typeof transceiver.setCodecPreferences !== 'function') return;
  const RtpSender = (globalThis as { RTCRtpSender?: typeof RTCRtpSender }).RTCRtpSender;
  const caps = RtpSender?.getCapabilities?.('video');
  if (!caps) return;
  const h264 = caps.codecs.filter((c) => c.mimeType.toLowerCase() === 'video/h264');
  if (h264.length === 0) return;
  const rest = caps.codecs.filter((c) => c.mimeType.toLowerCase() !== 'video/h264');
  transceiver.setCodecPreferences([...h264, ...rest]);
}

/** WHIP: publish a local `MediaStream` (from `getUserMedia`). Sendonly
 * transceivers, H.264 preferred on the video one. */
export async function startWhip(opts: WhipOptions): Promise<WebRtcSession> {
  const pcFactory = opts.pcFactory ?? (() => new RTCPeerConnection());
  const fetchImpl = opts.fetchImpl ?? ((input: string, init?: RequestInit) => fetch(input, init));
  const pc = pcFactory();

  const videoTrack = opts.stream.getVideoTracks()[0];
  const audioTrack = opts.stream.getAudioTracks()[0];

  if (videoTrack) {
    const transceiver = pc.addTransceiver(videoTrack, { direction: 'sendonly' });
    preferH264(transceiver);
  }
  if (audioTrack) {
    pc.addTransceiver(audioTrack, { direction: 'sendonly' });
  }

  const sessionUrl = await negotiate({
    pc,
    url: opts.url,
    token: opts.token,
    iceGatheringTimeoutMs: opts.iceGatheringTimeoutMs ?? DEFAULT_ICE_GATHERING_TIMEOUT_MS,
    fetchImpl,
  });

  return { pc, sessionUrl, stop: () => stopSession(pc, sessionUrl, fetchImpl) };
}

/** Playout delay estimate for a WHEP session, from `getStats()`'s
 * inbound-rtp `jitterBufferDelay / jitterBufferEmittedCount` (the spec's own
 * formula for average jitter buffer delay), in milliseconds. This is a
 * *buffer* estimate, not glass-to-glass latency: it excludes encode, network
 * and decode/render time. Returns null until at least one frame has been
 * emitted. Prefers the video row when both video and audio report. */
export async function measureJitterBufferMs(pc: RTCPeerConnection): Promise<number | null> {
  const stats = await pc.getStats();
  let best: number | null = null;
  let bestIsVideo = false;
  stats.forEach((report: RTCStats & { kind?: string; jitterBufferDelay?: number; jitterBufferEmittedCount?: number }) => {
    if (report.type !== 'inbound-rtp') return;
    const { jitterBufferDelay, jitterBufferEmittedCount } = report;
    if (typeof jitterBufferDelay !== 'number' || typeof jitterBufferEmittedCount !== 'number') return;
    if (jitterBufferEmittedCount <= 0) return;
    const ms = (jitterBufferDelay / jitterBufferEmittedCount) * 1000;
    const isVideo = report.kind === 'video';
    if (best === null || (isVideo && !bestIsVideo)) {
      best = ms;
      bestIsVideo = isVideo;
    }
  });
  return best;
}
