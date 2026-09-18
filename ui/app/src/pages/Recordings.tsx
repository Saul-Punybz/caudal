import { useState } from 'react';
import { listRecordings, requestClip, vodPlaylistUrl, type Recording, type RecordingTrack } from '../api';
import { usePolling } from '../hooks/usePolling';
import { CopyRow } from '../components/CopyRow';
import { EmptyPanel } from '../components/EmptyPanel';
import { ErrorBanner, ErrorState } from '../components/ErrorState';
import { formatBytes, formatCount, formatDurationSecs } from '../format';

/** Recordings (`crates/caudal-record`): closed-caption-free VOD copies of
 * whatever published, segmented as they go, playable while still live. */
export function Recordings() {
  const { data: recordings, error, loading } = usePolling(listRecordings, 3000);

  return (
    <main className="flex min-w-0 flex-grow flex-col gap-5 py-5 pl-2 pr-6">
      <header>
        <h1 className="display m-0 text-4xl leading-none">Recordings</h1>
        <p className="mt-1 text-sm text-on-surface-variant">VOD copies of what was published, by stream.</p>
      </header>

      {loading && !recordings && (
        <div aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </div>
      )}

      {!recordings && error && <ErrorState message={error.message} />}

      {recordings && (
        <>
          {error && <ErrorBanner message={error.message} />}
          {recordings.length === 0 ? (
            <EmptyPanel icon="video_library" title="No recordings yet">
              Enable <code className="num">[record]</code> in the server config and publish a stream — closed segments show up
              here as they're written.
            </EmptyPanel>
          ) : (
            <div className="flex flex-col gap-5">
              {groupByStream(recordings).map(([stream, items]) => (
                <section key={stream} aria-label={stream} className="flex flex-col gap-2">
                  <h2 className="m-0 text-lg font-medium">{stream}</h2>
                  <div className="flex flex-col gap-2">
                    {items.map((r) => (
                      <RecordingCard key={r.id} recording={r} />
                    ))}
                  </div>
                </section>
              ))}
            </div>
          )}
        </>
      )}
    </main>
  );
}

function groupByStream(recordings: Recording[]): [string, Recording[]][] {
  const byStream = new Map<string, Recording[]>();
  for (const r of recordings) {
    const list = byStream.get(r.stream);
    if (list) list.push(r);
    else byStream.set(r.stream, [r]);
  }
  return [...byStream.entries()];
}

function summarizeRecordingTrack(t: RecordingTrack): string {
  const codec = t.codec.toUpperCase();
  if (t.kind === 'video') {
    const dims = t.width && t.height ? `${t.width}x${t.height}` : null;
    return [codec, dims].filter(Boolean).join(' ');
  }
  if (t.kind === 'audio') {
    const rate = t.sample_rate ? `${(t.sample_rate / 1000).toFixed(0)} kHz` : null;
    return [codec, rate].filter(Boolean).join(' ');
  }
  return codec;
}

function RecordingCard({ recording }: { recording: Recording }) {
  const [downloading, setDownloading] = useState(false);
  const [downloadError, setDownloadError] = useState<string | null>(null);
  const live = recording.ended_at === null;
  const started = new Date(recording.started_at);
  const startedLabel = Number.isNaN(started.getTime()) ? recording.started_at : started.toLocaleString();

  async function download() {
    setDownloading(true);
    setDownloadError(null);
    try {
      const blob = await requestClip({
        stream: recording.stream,
        id: recording.id,
        fromMs: 0,
        toMs: recording.duration_ms,
      });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `${recording.stream}-${recording.id}.mp4`;
      document.body.appendChild(a);
      a.click();
      a.remove();
      URL.revokeObjectURL(url);
    } catch (err) {
      setDownloadError((err as Error).message);
    } finally {
      setDownloading(false);
    }
  }

  return (
    <div className="flex flex-col gap-3 rounded-lg bg-surface-container-low p-4">
      <div className="flex flex-wrap items-center gap-3">
        <span className="num font-semibold">{startedLabel}</span>
        {live ? (
          <span className="inline-flex h-6 items-center gap-1.5 rounded-full bg-primary-container px-2.5 text-xs font-bold tracking-wide text-on-primary-container">
            <span className="ms ms-fill text-sm" aria-hidden="true">
              fiber_manual_record
            </span>
            Recording
          </span>
        ) : (
          <span className="num text-sm text-on-surface-variant">{formatDurationSecs(recording.duration_ms / 1000)}</span>
        )}
        <span className="num text-sm text-on-surface-variant">{formatBytes(recording.bytes)}</span>
        <span className="num text-sm text-on-surface-variant">{formatCount(recording.segments)} segments</span>
        <div className="flex-grow" />
        <button
          type="button"
          onClick={() => void download()}
          disabled={downloading || live}
          title={live ? 'Finish recording before downloading' : undefined}
          className="state-layer flex h-9 items-center gap-1.5 rounded-full border border-outline px-4 text-sm font-semibold text-on-surface disabled:cursor-not-allowed disabled:opacity-50"
        >
          <span className={`ms text-lg ${downloading ? 'animate-spin' : ''}`} aria-hidden="true">
            {downloading ? 'sync' : 'download'}
          </span>
          {downloading ? 'Downloading…' : 'Download'}
        </button>
      </div>

      {recording.tracks.length > 0 && (
        <div className="num text-sm text-on-surface-variant">{recording.tracks.map(summarizeRecordingTrack).join(' · ')}</div>
      )}

      <CopyRow icon="stream" label="Play (HLS VOD)" url={vodPlaylistUrl(recording.stream, recording.id)} />

      {recording.error && (
        <div className="flex items-center gap-2 rounded-md bg-error-container px-3 py-2 text-sm text-on-error-container">
          <span className="ms text-lg" aria-hidden="true">
            warning
          </span>
          Stopped early: {recording.error}
        </div>
      )}

      {downloadError && (
        <div className="flex items-center gap-2 rounded-md bg-error-container px-3 py-2 text-sm text-on-error-container">
          <span className="ms text-lg" aria-hidden="true">
            warning
          </span>
          Download failed: {downloadError}
        </div>
      )}
    </div>
  );
}
