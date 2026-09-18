import { useState } from 'react';
import { ApiError, skipChannel, listChannels, type ChannelState, type ChannelStatus } from '../api';
import { usePolling } from '../hooks/usePolling';
import { EmptyPanel } from '../components/EmptyPanel';
import { ErrorBanner, ErrorState } from '../components/ErrorState';
import { StatusChip } from '../components/StatusChip';
import { formatDurationSecs } from '../format';

/** 24/7 linear channels (`crates/caudal-channel`): a playlist of files
 * played on a wall clock, republished as an ordinary live stream. */
export function Channels() {
  const { data: channels, error, loading } = usePolling(listChannels, 2000);

  return (
    <main className="flex min-w-0 flex-grow flex-col gap-5 py-5 pl-2 pr-6">
      <header>
        <h1 className="display m-0 text-4xl leading-none">Channels</h1>
        <p className="mt-1 text-sm text-on-surface-variant">24/7 linear channels playing from files.</p>
      </header>

      {loading && !channels && (
        <div aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </div>
      )}

      {!channels && error && <ErrorState message={error.message} />}

      {channels && (
        <>
          {error && <ErrorBanner message={error.message} />}
          {channels.length === 0 ? (
            <EmptyPanel icon="live_tv" title="No 24/7 channels configured">
              Add a <code className="num">[[channel]]</code> section to the server config — <code className="num">name</code>,{' '}
              <code className="num">items</code> (files or directories), <code className="num">loop</code> and{' '}
              <code className="num">shuffle</code> — and it will publish here as an ordinary live stream.
            </EmptyPanel>
          ) : (
            <div className="flex flex-col gap-3">
              {channels.map((c) => (
                <ChannelCard key={c.name} channel={c} />
              ))}
            </div>
          )}
        </>
      )}
    </main>
  );
}

function stateTone(state: ChannelState): { tone: 'primary' | 'error' | 'neutral'; icon: string; label: string } {
  if (state === 'playing') return { tone: 'primary', icon: 'play_circle', label: 'Playing' };
  if (state === 'error') return { tone: 'error', icon: 'error', label: 'Error' };
  return { tone: 'neutral', icon: 'pause_circle', label: 'Idle' };
}

function ChannelCard({ channel }: { channel: ChannelStatus }) {
  const [busy, setBusy] = useState(false);
  const [skipError, setSkipError] = useState<{ message: string; needsToken: boolean } | null>(null);
  const [token, setToken] = useState('');
  const { tone, icon, label } = stateTone(channel.state);

  async function doSkip(withToken?: string) {
    setBusy(true);
    try {
      await skipChannel(channel.name, withToken);
      setSkipError(null);
    } catch (err) {
      if (err instanceof ApiError && err.status === 401) {
        setSkipError({ message: 'This channel needs a token to skip.', needsToken: true });
      } else if (err instanceof ApiError && err.status === 403) {
        setSkipError({ message: 'Token refused.', needsToken: true });
      } else {
        setSkipError({ message: (err as Error).message, needsToken: false });
      }
    } finally {
      setBusy(false);
    }
  }

  const now = channel.now_playing;
  const position = now ? formatDurationSecs(now.position_secs) : null;
  const duration = now?.duration_secs != null ? formatDurationSecs(now.duration_secs) : null;
  const fileName = now ? now.path.split('/').pop() || now.path : null;

  return (
    <div className="flex flex-col gap-2 rounded-lg bg-surface-container-low p-4">
      <div className="flex flex-wrap items-center gap-3">
        <span className="min-w-0 flex-shrink-0 font-semibold">{channel.name}</span>
        <StatusChip tone={tone} icon={icon} label={label} />
        <span className="num text-xs text-on-surface-variant">
          item {Math.min(channel.index + 1, Math.max(channel.items, 1))} of {channel.items}
        </span>
        <div className="flex-grow" />
        <button
          type="button"
          onClick={() => void doSkip(token || undefined)}
          disabled={busy}
          className="state-layer flex h-9 items-center gap-1.5 rounded-full border border-outline px-4 text-sm font-semibold text-on-surface disabled:cursor-not-allowed disabled:opacity-50"
        >
          <span className={`ms text-lg ${busy ? 'animate-spin' : ''}`} aria-hidden="true">
            {busy ? 'sync' : 'skip_next'}
          </span>
          Skip
        </button>
      </div>

      <div className="num flex items-center gap-2 text-sm text-on-surface-variant">
        <span className="ms text-base" aria-hidden="true">
          movie
        </span>
        {fileName ? (
          <span className="truncate">
            {fileName}
            {duration ? ` — ${position} / ${duration}` : position ? ` — ${position}` : ''}
          </span>
        ) : (
          'Nothing playing yet'
        )}
      </div>

      {channel.error && (
        <div className="flex items-center gap-2 rounded-md bg-error-container px-3 py-2 text-sm text-on-error-container">
          <span className="ms text-lg" aria-hidden="true">
            warning
          </span>
          Last error: {channel.error}
        </div>
      )}

      {skipError && (
        <div className="flex flex-wrap items-center gap-2 rounded-md bg-error-container px-3 py-2 text-sm text-on-error-container">
          <span className="ms text-lg" aria-hidden="true">
            warning
          </span>
          {skipError.message}
          {skipError.needsToken && (
            <form
              className="flex items-center gap-2"
              onSubmit={(e) => {
                e.preventDefault();
                void doSkip(token || undefined);
              }}
            >
              <input
                type="text"
                value={token}
                onChange={(e) => setToken(e.target.value)}
                placeholder="Publish token"
                aria-label={`Publish token for ${channel.name}`}
                autoComplete="off"
                className="h-8 rounded-sm border border-outline bg-transparent px-2.5 text-xs text-on-error-container outline-none"
              />
              <button type="submit" className="state-layer h-8 rounded-full bg-primary px-3 text-xs font-semibold text-on-primary">
                Retry
              </button>
            </form>
          )}
        </div>
      )}
    </div>
  );
}
