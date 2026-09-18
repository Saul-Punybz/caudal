import { useEffect, useRef, useState } from 'react';
import { Link, useParams } from 'react-router-dom';
import { getStream } from '../api';
import { bitrateFromDelta, formatBitrate } from '../bitrate';
import { usePolling } from '../hooks/usePolling';
import { LiveBadge } from '../components/LiveBadge';
import { Player } from '../components/Player';
import { CopyRow } from '../components/CopyRow';
import { ErrorBanner, ErrorState } from '../components/ErrorState';
import { formatCount, formatSeconds, summarizeTrack } from '../format';

type Tab = 'outputs' | 'tracks' | 'health';

export function StreamDetail() {
  const { name = '' } = useParams<{ name: string }>();
  const { data: stream, error } = usePolling(() => getStream(name), 1000);
  const [tab, setTab] = useState<Tab>('outputs');
  const [latency, setLatency] = useState<number | null>(null);

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
          <Player streamName={stream.name} onLatency={setLatency} />
          <div className="flex flex-wrap gap-3 text-sm text-on-surface-variant">
            <span className="num rounded-full bg-surface-container-high px-3 py-1">
              {latency !== null ? `${latency.toFixed(1)} s behind live` : 'measuring latency…'}
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
              <CopyRow icon="podcasts" label="WebRTC" tag="coming soon" url="not yet available" disabled />
              <CopyRow icon="bolt" label="Media over QUIC" tag="coming soon" url="not yet available" disabled />
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
