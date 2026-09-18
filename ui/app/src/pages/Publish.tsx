import { useEffect, useRef, useState } from 'react';
import type { ReactNode } from 'react';
import { Link } from 'react-router-dom';
import { WebRtcError, startWhip, type WebRtcSession } from '../webrtc';
import { CopyRow } from '../components/CopyRow';

type Status =
  | { kind: 'idle' }
  | { kind: 'connecting' }
  | { kind: 'live' }
  | { kind: 'error'; message: string; status?: number };

/** Publish a webcam over WHIP. Grabs the camera/mic on mount (needed to
 * both preview and list labeled devices), lets the user swap either, then
 * negotiates a WHIP session against `<origin>/whip/<name>` on "Go live". */
export function Publish() {
  const videoRef = useRef<HTMLVideoElement>(null);
  const previewStreamRef = useRef<MediaStream | null>(null);
  const sessionRef = useRef<WebRtcSession | null>(null);

  const [streamName, setStreamName] = useState('');
  const [token, setToken] = useState('');
  const [videoDeviceId, setVideoDeviceId] = useState<string>('');
  const [audioDeviceId, setAudioDeviceId] = useState<string>('');
  const [devices, setDevices] = useState<MediaDeviceInfo[]>([]);
  const [mediaError, setMediaError] = useState<string | null>(null);
  const [status, setStatus] = useState<Status>({ kind: 'idle' });

  // Acquire the camera/mic once on mount so the preview and device pickers
  // have something real to show — no fake device list or placeholder frame.
  useEffect(() => {
    let cancelled = false;
    async function openDefaultDevices() {
      try {
        const stream = await navigator.mediaDevices.getUserMedia({ video: true, audio: true });
        if (cancelled) {
          stream.getTracks().forEach((t) => t.stop());
          return;
        }
        applyStream(stream);
        const list = await navigator.mediaDevices.enumerateDevices();
        if (cancelled) return;
        setDevices(list);
        setVideoDeviceId(stream.getVideoTracks()[0]?.getSettings().deviceId ?? '');
        setAudioDeviceId(stream.getAudioTracks()[0]?.getSettings().deviceId ?? '');
      } catch (err) {
        if (!cancelled) setMediaError((err as Error).message);
      }
    }
    void openDefaultDevices();
    return () => {
      cancelled = true;
      previewStreamRef.current?.getTracks().forEach((t) => t.stop());
      if (sessionRef.current) void sessionRef.current.stop();
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function applyStream(stream: MediaStream) {
    previewStreamRef.current?.getTracks().forEach((t) => t.stop());
    previewStreamRef.current = stream;
    if (videoRef.current) videoRef.current.srcObject = stream;
  }

  async function switchDevice(kind: 'video' | 'audio', deviceId: string) {
    setMediaError(null);
    try {
      const constraints: MediaStreamConstraints = {
        video: { deviceId: kind === 'video' ? { exact: deviceId } : videoDeviceId ? { exact: videoDeviceId } : undefined },
        audio: { deviceId: kind === 'audio' ? { exact: deviceId } : audioDeviceId ? { exact: audioDeviceId } : undefined },
      };
      const stream = await navigator.mediaDevices.getUserMedia(constraints);
      applyStream(stream);
      if (kind === 'video') setVideoDeviceId(deviceId);
      else setAudioDeviceId(deviceId);
    } catch (err) {
      setMediaError((err as Error).message);
    }
  }

  async function goLive() {
    const name = streamName.trim();
    const stream = previewStreamRef.current;
    if (!name || !stream) return;
    setStatus({ kind: 'connecting' });
    try {
      const origin = window.location.origin;
      const session = await startWhip({
        url: `${origin}/whip/${encodeURIComponent(name)}`,
        token: token.trim() || undefined,
        stream,
      });
      sessionRef.current = session;
      setStatus({ kind: 'live' });
    } catch (err) {
      if (err instanceof WebRtcError) {
        const message = err.status === 401 ? 'token needed' : err.status === 403 ? 'token refused' : err.message;
        setStatus({ kind: 'error', message, status: err.status });
      } else {
        setStatus({ kind: 'error', message: (err as Error).message });
      }
    }
  }

  async function stopLive() {
    const session = sessionRef.current;
    sessionRef.current = null;
    setStatus({ kind: 'idle' });
    if (session) await session.stop();
  }

  const live = status.kind === 'live';
  const connecting = status.kind === 'connecting';
  const videoInputs = devices.filter((d) => d.kind === 'videoinput');
  const audioInputs = devices.filter((d) => d.kind === 'audioinput');
  const publishedName = streamName.trim();

  return (
    <main className="mx-auto flex min-w-0 max-w-3xl flex-grow flex-col gap-5 overflow-y-auto p-6">
      <header>
        <h1 className="display m-0 text-3xl leading-none">Publish</h1>
        <p className="mt-1 text-sm text-on-surface-variant">
          Send your camera and mic to Caudal over WHIP (WebRTC).
        </p>
      </header>

      <div className="relative aspect-video w-full overflow-hidden rounded-lg bg-[#0b0d1b]">
        <video ref={videoRef} autoPlay muted playsInline className="h-full w-full" aria-label="Camera preview" />
        {mediaError && (
          <div
            role="alert"
            className="absolute inset-0 flex items-center justify-center bg-black/70 p-4 text-center text-sm text-white"
          >
            Couldn't open the camera/mic: {mediaError}
          </div>
        )}
      </div>

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field label="Camera">
          <select
            aria-label="Camera"
            value={videoDeviceId}
            disabled={live || connecting}
            onChange={(e) => void switchDevice('video', e.target.value)}
            className="h-12 w-full rounded-sm border border-outline bg-transparent px-3.5 text-sm text-on-surface outline-none disabled:opacity-50"
          >
            {videoInputs.length === 0 && <option value="">No camera found</option>}
            {videoInputs.map((d) => (
              <option key={d.deviceId} value={d.deviceId}>
                {d.label || `Camera ${d.deviceId.slice(0, 6)}`}
              </option>
            ))}
          </select>
        </Field>
        <Field label="Microphone">
          <select
            aria-label="Microphone"
            value={audioDeviceId}
            disabled={live || connecting}
            onChange={(e) => void switchDevice('audio', e.target.value)}
            className="h-12 w-full rounded-sm border border-outline bg-transparent px-3.5 text-sm text-on-surface outline-none disabled:opacity-50"
          >
            {audioInputs.length === 0 && <option value="">No microphone found</option>}
            {audioInputs.map((d) => (
              <option key={d.deviceId} value={d.deviceId}>
                {d.label || `Microphone ${d.deviceId.slice(0, 6)}`}
              </option>
            ))}
          </select>
        </Field>
      </div>

      <div className="grid grid-cols-1 gap-4 sm:grid-cols-2">
        <Field label="Stream name">
          <input
            type="text"
            value={streamName}
            disabled={live || connecting}
            onChange={(e) => setStreamName(e.target.value)}
            placeholder="e.g. my-show"
            aria-label="Stream name"
            className="h-12 w-full rounded-sm border border-outline bg-transparent px-3.5 text-sm text-on-surface outline-none disabled:opacity-50"
          />
        </Field>
        <Field label="Token (optional)">
          <input
            type="text"
            value={token}
            disabled={live || connecting}
            onChange={(e) => setToken(e.target.value)}
            placeholder="only if this stream needs one"
            aria-label="Publish token"
            autoComplete="off"
            className="h-12 w-full rounded-sm border border-outline bg-transparent px-3.5 text-sm text-on-surface outline-none disabled:opacity-50"
          />
        </Field>
      </div>

      <ObsHelper streamName={streamName} token={token} />

      <div className="flex flex-wrap items-center gap-3">
        {!live ? (
          <button
            type="button"
            onClick={() => void goLive()}
            disabled={!streamName.trim() || !previewStreamRef.current || connecting}
            className="state-layer h-12 rounded-full bg-primary px-6 text-sm font-semibold text-on-primary disabled:cursor-not-allowed disabled:opacity-50"
          >
            {connecting ? 'Connecting…' : 'Go live'}
          </button>
        ) : (
          <button
            type="button"
            onClick={() => void stopLive()}
            className="state-layer h-12 rounded-full border border-outline px-6 text-sm font-semibold text-on-surface"
          >
            Stop
          </button>
        )}

        <StatusLine status={status} />

        {live && publishedName && (
          <Link
            to={`/streams/${encodeURIComponent(publishedName)}`}
            className="state-layer flex items-center gap-1 rounded-full px-3 py-2 text-sm font-semibold text-primary no-underline"
          >
            View {publishedName}
            <span className="ms text-lg" aria-hidden="true">
              arrow_forward
            </span>
          </Link>
        )}
      </div>
    </main>
  );
}

/** OBS 30+ can publish over WHIP natively (Settings → Stream → Service:
 * WHIP), which just wants the same server URL and bearer token this page
 * already negotiates with. Shown as plain copy rows rather than a whole
 * second form, since the values are exactly the "Stream name"/"Token"
 * fields above. */
function ObsHelper({ streamName, token }: { streamName: string; token: string }) {
  const name = streamName.trim();
  const origin = typeof window !== 'undefined' ? window.location.origin : '';
  const whipUrl = name ? `${origin}/whip/${encodeURIComponent(name)}` : '';

  return (
    <div className="flex flex-col gap-2 rounded-lg bg-surface-container-low p-4">
      <div className="flex items-center gap-2 text-sm font-semibold">
        <span className="ms text-lg" aria-hidden="true">
          videocam
        </span>
        Copy for OBS (WHIP output, OBS 30+)
      </div>
      <p className="m-0 text-xs text-on-surface-variant">
        In OBS: Settings → Stream → Service: <span className="font-semibold">WHIP</span>. Paste the server URL and, if this
        stream needs one, the bearer token below.
      </p>
      <CopyRow
        icon="dns"
        label="Server"
        url={whipUrl || 'set a stream name above first'}
        disabled={!whipUrl}
      />
      <CopyRow
        icon="key"
        label="Bearer Token"
        url={token.trim() || 'no token set for this stream'}
        disabled={!token.trim()}
      />
    </div>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <label className="flex flex-col gap-1.5 text-sm">
      <span className="font-medium text-on-surface-variant">{label}</span>
      {children}
    </label>
  );
}

function StatusLine({ status }: { status: Status }) {
  if (status.kind === 'idle') return null;
  const icon = status.kind === 'live' ? 'radio_button_checked' : status.kind === 'error' ? 'error' : 'sync';
  const color = status.kind === 'live' ? 'text-primary' : status.kind === 'error' ? 'text-error' : 'text-on-surface-variant';
  const text =
    status.kind === 'connecting'
      ? 'Connecting…'
      : status.kind === 'live'
        ? 'Live'
        : `Error${status.status ? ` (${status.status})` : ''}: ${status.message}`;
  return (
    <span role="status" aria-live="polite" className={`flex items-center gap-1.5 text-sm font-medium ${color}`}>
      <span className={`ms ms-fill text-lg ${status.kind === 'connecting' ? 'animate-spin' : ''}`} aria-hidden="true">
        {icon}
      </span>
      {text}
    </span>
  );
}
