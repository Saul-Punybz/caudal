import { listRestreams, type RestreamState, type RestreamStatus } from '../api';
import { usePolling } from '../hooks/usePolling';
import { EmptyPanel } from '../components/EmptyPanel';
import { ErrorBanner, ErrorState } from '../components/ErrorState';
import { StatusChip, type ChipTone } from '../components/StatusChip';
import { formatBytes, formatDurationSecs } from '../format';

/** Restream targets (`crates/caudal-restream`): pushes a local stream out
 * to another RTMP/RTMPS endpoint (YouTube, Twitch, another Caudal, …). */
export function Restreams() {
  const { data: targets, error, loading } = usePolling(listRestreams, 1000);

  return (
    <main className="flex min-w-0 flex-grow flex-col gap-5 py-5 pl-2 pr-6">
      <header>
        <h1 className="display m-0 text-4xl leading-none">Restreams</h1>
        <p className="mt-1 text-sm text-on-surface-variant">Push targets this server is multistreaming to.</p>
      </header>

      {loading && !targets && (
        <div aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </div>
      )}

      {!targets && error && <ErrorState message={error.message} />}

      {targets && (
        <>
          {error && <ErrorBanner message={error.message} />}
          {targets.length === 0 ? (
            <EmptyPanel icon="cast" title="No restream targets configured">
              Add a <code className="num">[[restream]]</code> section to the server config — <code className="num">stream</code>{' '}
              (which local stream to push) and <code className="num">url</code> (the RTMP/RTMPS destination) — and it will show
              up here.
            </EmptyPanel>
          ) : (
            <div role="table" aria-label="Restream targets" className="flex flex-col overflow-hidden rounded-lg bg-surface-container-low">
              <div
                role="row"
                className="grid grid-cols-[1fr_1.6fr_0.9fr_0.9fr_0.7fr_1.4fr] gap-3 border-b border-outline-variant px-5 py-2 text-xs font-medium uppercase tracking-wide text-on-surface-variant"
              >
                <span role="columnheader">Stream</span>
                <span role="columnheader">Target</span>
                <span role="columnheader">State</span>
                <span role="columnheader" className="text-right">
                  Sent
                </span>
                <span role="columnheader" className="text-right">
                  Since
                </span>
                <span role="columnheader">Last error</span>
              </div>
              {targets.map((t, i) => (
                <RestreamRow key={`${t.stream}-${t.target}-${i}`} target={t} />
              ))}
            </div>
          )}
        </>
      )}
    </main>
  );
}

function stateChip(state: RestreamState): { tone: ChipTone; icon: string; label: string; spin?: boolean } {
  switch (state) {
    case 'live':
      return { tone: 'primary', icon: 'radio_button_checked', label: 'Live' };
    case 'connecting':
      return { tone: 'tertiary', icon: 'sync', label: 'Connecting', spin: true };
    case 'retrying':
      return { tone: 'error', icon: 'sync_problem', label: 'Retrying' };
    case 'waiting':
    default:
      return { tone: 'neutral', icon: 'schedule', label: 'Waiting' };
  }
}

function RestreamRow({ target }: { target: RestreamStatus }) {
  const chip = stateChip(target.state);
  return (
    <div
      role="row"
      className="grid grid-cols-[1fr_1.6fr_0.9fr_0.9fr_0.7fr_1.4fr] items-center gap-3 border-b border-outline-variant px-5 text-sm last:border-b-0"
      style={{ minHeight: 56 }}
    >
      <span role="cell" className="truncate font-semibold">
        {target.stream}
      </span>
      <span role="cell" className="num truncate text-on-surface-variant" title={target.target}>
        {target.target}
      </span>
      <span role="cell">
        <StatusChip tone={chip.tone} icon={chip.icon} label={chip.label} spin={chip.spin} />
      </span>
      <span role="cell" className="num text-right">
        {formatBytes(target.bytes_sent)}
      </span>
      <span role="cell" className="num text-right">
        {formatDurationSecs(target.since_secs)}
      </span>
      <span role="cell" className="truncate text-xs text-on-surface-variant" title={target.last_error ?? undefined}>
        {target.last_error ?? '—'}
      </span>
    </div>
  );
}
