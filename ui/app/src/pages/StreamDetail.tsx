import { useEffect, useRef, useState } from 'react';
import { Link, useParams, useSearchParams } from 'react-router-dom';
import { getMoqFingerprint, getStream } from '../api';
import { bitrateFromDelta, formatBitrate } from '../bitrate';
import { usePolling } from '../hooks/usePolling';
import { LiveBadge } from '../components/LiveBadge';
import { Player, type AuthErrorKind } from '../components/Player';
import { WebRtcPlayer } from '../components/WebRtcPlayer';
import { MoqPlayer } from '../components/MoqPlayer';
import { CopyRow } from '../components/CopyRow';
import { ErrorBanner, ErrorState } from '../components/ErrorState';
import { formatCount, formatSeconds, summarizeTrack } from '../format';

type Tab = 'outputs' | 'tracks' | 'health';
type PlaybackMode = 'hls' | 'webrtc' | 'moq';

export function StreamDetail() {
  const { name = '' } = useParams<{ name: string }>();
  const [searchParams] = useSearchParams();
  const { data: stream, error } = usePolling(() => getStream(name), 1000);
  // The relay's cert is rotated every so often (see STATUS.md "Batch 5"), not
  // every second, but polling slowly here also means the MoQ option starts
  // working without a page reload once the server adds support.
  const { data: moq, error: moqError } = usePolling(() => getMoqFingerprint(), 5000);
  const moqSupported = typeof window !== 'undefined' && 'WebTransport' in window;
  const [tab, setTab] = useState<Tab>('outputs');
  const [mode, setMode] = useState<PlaybackMode>('hls');
  const [latency, setLatency] = useState<number | null>(null);
  const [authError, setAuthError] = useState<AuthErrorKind | null>(null);
  const [token, setToken] = useState<string | undefined>(() => searchParams.get('token') ?? undefined);

  // Track frames_in across polls to report whether ingest is still moving,
  // and bytes_in to compute a live bitrate — same technique as the Overview
  // table, scoped to just this one stream.
  const prevFrames = useRef<{ framesIn: number; atMs: number } | null>(null);
  const prevBytes = useRef<{ bytesIn: number; atMs: number } | null>(null);
  const [framesRising, setFramesRising] = useState<boolean | null>(null);
  const [bitrate, setBitrate] = useState<number | null>(null);

  useEffect(() => {
    if (!stream) return;
    const now = Date.now();
    const framesSample = { framesIn: stream.stats.frames_in, atMs: now };
    if (prevFrames.current) {
      setFramesRising(framesSample.framesIn > prevFrames.current.framesIn);
    }
    prevFrames.current = framesSample;

    const bytesSample = { bytesIn: stream.stats.bytes_in, atMs: now };
    setBitrate(bitrateFromDelta(prevBytes.current ?? undefined, bytesSample));
    prevBytes.current = bytesSample;
  }, [stream]);

  // Reset per-stream trackers when navigating to a different stream.
  useEffect(() => {
    prevFrames.current = null;
    prevBytes.current = null;
    setFramesRising(null);
    setBitrate(null);
    setTab('outputs');
    setMode('hls');
    setAuthError(null);
    setLatency(null);
  }, [name]);

  if (!stream && error) {
    return (
      <main className="flex min-w-0 flex-grow flex-col gap-5 p-6">
        <BackLink />
        <ErrorState message={error.message} />
      </main>
    );
  }

  if (!stream) {
    return (
      <main className="flex min-w-0 flex-grow flex-col gap-5 p-6">
        <BackLink />
        <div aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </div>
      </main>
    );
  }

  const host = typeof window !== 'undefined' ? window.location.hostname : 'caudal.example';
  const origin = typeof window !== 'undefined' ? window.location.origin : `http://${host}:8080`;
  const rtmpUrl = `rtmp://${host}:1935/live/${stream.name}`;
  const hlsUrl = `${origin}/hls/${encodeURIComponent(stream.name)}/master.m3u8`;
  const whepUrl = `${origin}/whep/${encodeURIComponent(stream.name)}`;
  // RTMP/SRT sources without Opus arrive as AAC; WHEP can't carry that
  // without transcoding (see STATUS.md "Batch 4"), so the stream is
  // video-only over WebRTC. Said plainly rather than silently dropping audio.
  const aacOnly = stream.tracks.some((t) => t.kind === 'audio' && t.codec.toLowerCase().includes('aac'));

  return (
    <main className="flex min-w-0 flex-grow flex-col gap-4 p-6">
      <header className="flex flex-wrap items-center gap-3">
        <BackLink />
        <div className="flex flex-col">
          <span className="text-xs text-on-surface-variant">Streams</span>
          <h1 className="display m-0 text-3xl leading-none">{stream.name}</h1>
        </div>
        <LiveBadge live />
      </header>

      {error && <ErrorBanner message={error.message} />}

      <div className="flex min-h-0 flex-grow flex-col gap-4 lg:flex-row">
        <div className="flex min-w-0 flex-grow flex-col gap-4">
          <div className="flex items-center justify-between gap-3">
            <ModeSwitch
              mode={mode}
              moqDisabled={!moqSupported}
              onChange={(m) => {
                setMode(m);
                setAuthError(null);
                setLatency(null);
              }}
            />
            {mode === 'webrtc' && aacOnly && (
              <span className="flex items-center gap-1.5 text-xs text-on-surface-variant">
                <span className="ms text-base" aria-hidden="true">
                  info
                </span>
                audio not available over WebRTC for this stream (AAC source)
              </span>
            )}
          </div>

          {authError ? (
            <TokenPrompt
              refused={authError === 'refused'}
              onSubmit={(t) => {
                setToken(t);
                setAuthError(null);
              }}
            />
          ) : mode === 'hls' ? (
            <Player streamName={stream.name} token={token} onLatency={setLatency} onAuthError={setAuthError} />
          ) : mode === 'webrtc' ? (
            <WebRtcPlayer url={whepUrl} token={token} onLatencyMs={setLatency} onAuthError={setAuthError} />
          ) : moq ? (
            <MoqPlayer
              relayUrl={moq.url}
              broadcastName={stream.name}
              fingerprint={moq.fingerprint}
              token={token}
              onLatencyMs={setLatency}
            />
          ) : (
            <div
              role="status"
              className="flex aspect-video w-full items-center justify-center rounded-lg bg-surface-container-high text-sm text-on-surface-variant"
            >
              {moqError ? "Media over QUIC isn't available on this server yet." : 'Loading MoQ endpoint…'}
            </div>
          )}

          <div aria-live="polite" className="flex flex-wrap gap-3 text-sm text-on-surface-variant">
            <span className="num rounded-full bg-surface-container-high px-3 py-1">
              {latency === null
                ? 'measuring latency…'
                : mode === 'hls'
                  ? `${latency.toFixed(1)} s behind live`
                  : mode === 'webrtc'
                    ? `≈ ${Math.round(latency)} ms buffer`
                    : `≈ ${Math.round(latency)} ms jitter buffer`}
            </span>
            <span className="num rounded-full bg-surface-container-high px-3 py-1">
              {formatCount(stream.stats.viewers)} viewers
            </span>
            <span className="num rounded-full bg-surface-container-high px-3 py-1">
              {formatBitrate(bitrate)} ingest
            </span>
          </div>
        </div>

        <aside
          aria-label="Stream details"
          className="flex w-full flex-col overflow-hidden rounded-lg bg-surface-container lg:w-[440px]"
        >
          <div role="tablist" aria-label="Detail sections" className="flex border-b border-outline-variant">
            <TabButton id="outputs" current={tab} onSelect={setTab} label="Outputs" />
            <TabButton id="tracks" current={tab} onSelect={setTab} label="Tracks" />
            <TabButton id="health" current={tab} onSelect={setTab} label="Health" />
          </div>

          {tab === 'outputs' && (
            <div role="tabpanel" className="flex flex-col gap-3 p-4">
              <div className="text-xs font-semibold uppercase tracking-wide text-on-surface-variant">
                Publish here
              </div>
              <CopyRow icon="input" label="RTMP · OBS, ffmpeg" url={rtmpUrl} />
              <div className="mt-1 text-xs font-semibold uppercase tracking-wide text-on-surface-variant">
                Play it
              </div>
              <CopyRow icon="stream" label="LL-HLS" tag="hls.js / Safari" url={hlsUrl} />
              <CopyRow
                icon="podcasts"
                label="WebRTC (WHEP)"
                tag={aacOnly ? 'POST · video only' : 'POST · H.264 + Opus'}
                url={whepUrl}
              />
              {moq ? (
                <>
                  <CopyRow icon="bolt" label="Media over QUIC relay" tag="WebTransport" url={moq.url} />
                  <CopyRow icon="podcasts" label="Broadcast name" tag="= stream name" url={stream.name} />
                  {moq.fingerprint && (
                    <CopyRow
                      icon="fingerprint"
                      label="Certificate fingerprint"
                      tag="sha-256, self-signed"
                      url={moq.fingerprint}
                    />
                  )}
                </>
              ) : (
                <CopyRow
                  icon="bolt"
                  label="Media over QUIC"
                  tag={moqError ? 'not available yet' : 'loading…'}
                  url="not yet available"
                  disabled
                />
              )}
              <CopyRow icon="swap_calls" label="SRT" tag="coming soon" url="not yet available" disabled />
            </div>
          )}

          {tab === 'tracks' && (
            <div role="tabpanel" className="flex flex-col gap-2 p-4">
              {stream.tracks.length === 0 && (
                <p className="text-sm text-on-surface-variant">No tracks reported.</p>
              )}
              {stream.tracks.map((t) => (
                <div key={t.id} className="flex gap-3 rounded-md bg-surface-container-high p-3.5">
                  <span className="ms text-primary" aria-hidden="true">
                    {t.kind === 'video' ? 'videocam' : t.kind === 'audio' ? 'graphic_eq' : 'description'}
                  </span>
                  <div>
                    <div className="font-semibold">
                      Track {t.id} · {t.kind}
                    </div>
                    <div className="num text-sm text-on-surface-variant">
                      {summarizeTrack(t)} · timescale {formatCount(t.timescale)}
                      {t.lang ? ` · ${t.lang}` : ''}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          )}

          {tab === 'health' && (
            <div role="tabpanel" className="flex flex-col gap-1 p-4">
              <HealthRow
                ok={framesRising}
                label="Ingest frames rising"
                detail={framesRising === null ? 'measuring…' : formatCount(stream.stats.frames_in)}
              />
              <HealthRow
                ok={stream.stats.buffered_ms > 0}
                label="Buffer holding data"
                detail={formatSeconds(stream.stats.buffered_ms)}
              />
            </div>
          )}
        </aside>
      </div>
    </main>
  );
}

function BackLink() {
  return (
    <Link
      to="/"
      aria-label="Back to overview"
      className="state-layer flex items-center justify-center rounded-full text-on-surface no-underline"
      style={{ width: 48, height: 48 }}
    >
      <span className="ms" aria-hidden="true">
        arrow_back
      </span>
    </Link>
  );
}

function TabButton({
  id,
  current,
  onSelect,
  label,
}: {
  id: Tab;
  current: Tab;
  onSelect: (t: Tab) => void;
  label: string;
}) {
  const selected = id === current;
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      onClick={() => onSelect(id)}
      className={`tab flex-grow border-0 bg-transparent text-sm font-semibold ${
        selected ? 'text-primary shadow-[inset_0_-3px_0_var(--md-sys-color-primary)]' : 'text-on-surface-variant'
      }`}
      style={{ height: 48 }}
    >
      {label}
    </button>
  );
}

/** Health rows never rely on color alone: icon + text + a status word. */
function HealthRow({ ok, label, detail }: { ok: boolean | null; label: string; detail: string }) {
  const icon = ok === null ? 'help' : ok ? 'check_circle' : 'warning';
  const color = ok === null ? 'text-on-surface-variant' : ok ? 'text-primary' : 'text-error';
  return (
    <div className="flex items-center gap-3" style={{ height: 48 }}>
      <span className={`ms ms-fill ${color}`} aria-hidden="true">
        {icon}
      </span>
      <span className="flex-grow">{label}</span>
      <span className="num text-sm text-on-surface-variant">{detail}</span>
    </div>
  );
}

/** M3 segmented button: a three-way choice between the low-latency HLS
 * player, the sub-second WebRTC one, and Media over QUIC. */
function ModeSwitch({
  mode,
  moqDisabled,
  onChange,
}: {
  mode: PlaybackMode;
  moqDisabled: boolean;
  onChange: (mode: PlaybackMode) => void;
}) {
  return (
    <div
      role="radiogroup"
      aria-label="Playback protocol"
      className="inline-flex h-10 overflow-hidden rounded-full border border-outline"
    >
      <SegButton label="LL-HLS" selected={mode === 'hls'} onSelect={() => onChange('hls')} />
      <SegButton label="WebRTC" selected={mode === 'webrtc'} onSelect={() => onChange('webrtc')} />
      <SegButton
        label="MoQ"
        selected={mode === 'moq'}
        onSelect={() => onChange('moq')}
        disabled={moqDisabled}
        disabledReason="needs WebTransport (Chrome, Edge, Firefox)"
      />
    </div>
  );
}

function SegButton({
  label,
  selected,
  onSelect,
  disabled = false,
  disabledReason,
}: {
  label: string;
  selected: boolean;
  onSelect: () => void;
  disabled?: boolean;
  disabledReason?: string;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-disabled={disabled}
      title={disabled ? disabledReason : undefined}
      onClick={disabled ? undefined : onSelect}
      disabled={disabled}
      className={`state-layer flex items-center gap-1.5 border-0 px-4 text-sm font-medium disabled:cursor-not-allowed disabled:opacity-50 ${
        selected ? 'bg-secondary-container text-on-secondary-container' : 'bg-transparent text-on-surface-variant'
      }`}
    >
      {selected && (
        <span className="ms text-lg" aria-hidden="true">
          check
        </span>
      )}
      {label}
    </button>
  );
}

/** Shown instead of a broken player on 401 ("token needed") or 403
 * ("token refused"). An M3 outlined text field with a floating label. */
function TokenPrompt({ refused, onSubmit }: { refused: boolean; onSubmit: (token: string) => void }) {
  const [value, setValue] = useState('');
  return (
    <div
      role="alert"
      className="flex aspect-video w-full flex-col items-center justify-center gap-4 rounded-lg bg-surface-container-high p-6 text-center"
    >
      <span className="ms text-4xl text-on-surface-variant" aria-hidden="true">
        {refused ? 'block' : 'lock'}
      </span>
      <p className="m-0 text-sm text-on-surface-variant">
        {refused ? 'Token refused.' : 'This stream needs a token to play.'}
      </p>
      <form
        className="flex w-full max-w-xs items-center gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          if (value.trim()) onSubmit(value.trim());
        }}
      >
        <label className="relative min-w-0 flex-grow">
          <span className="absolute -top-2 left-2.5 rounded-sm bg-surface-container-high px-1 text-xs text-on-surface-variant">
            Token
          </span>
          <input
            type="text"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            aria-label="Play token"
            autoComplete="off"
            className="h-12 w-full min-w-0 rounded-sm border border-outline bg-transparent px-3.5 text-sm text-on-surface outline-none focus-visible:border-2 focus-visible:border-primary"
          />
        </label>
        <button
          type="submit"
          className="state-layer h-12 flex-shrink-0 rounded-full bg-primary px-4 text-sm font-semibold text-on-primary"
        >
          Play
        </button>
      </form>
    </div>
  );
}
