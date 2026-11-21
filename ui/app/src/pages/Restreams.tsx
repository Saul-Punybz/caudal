import { listAutoPushes, listPushes, listRestreams, type AutoPushRule, type RestreamState, type RestreamStatus } from '../api';
import { ConfigTable } from '../components/ConfigTable';
import { usePolling } from '../hooks/usePolling';
import { EmptyPanel } from '../components/EmptyPanel';
import { ErrorBanner, ErrorState } from '../components/ErrorState';
import { StatusChip, type ChipTone } from '../components/StatusChip';
import { formatBytes, formatDurationSecs } from '../format';

/** Restream targets (`crates/caudal-restream`): pushes a local stream out
 * to another RTMP/RTMPS endpoint (YouTube, Twitch, another Caudal, …). */
export function Restreams() {
  const { data: targets, error, loading } = usePolling(listRestreams, 1000);
  // The config rarely changes; poll it slowly. Both lists stay hidden when
  // empty (or when the server runs without a config file).
  const { data: pushes } = usePolling(listPushes, 5000);
  const { data: rules } = usePolling(listAutoPushes, 5000);

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

      {pushes && pushes.length > 0 && (
        <section aria-label="Configured pushes" className="flex flex-col gap-2">
          <h2 className="m-0 text-xl font-medium">Configured pushes</h2>
          <p className="m-0 text-sm text-on-surface-variant">
            Every push in the config (RTMP, SRT, UDP/RTP), including <code className="num">[[push.auto]]</code> patterns.
          </p>
          <ConfigTable label="Configured pushes" entries={pushes} />
        </section>
      )}

      {rules && rules.length > 0 && (
        <section aria-label="Automatic push rules" className="flex flex-col gap-2">
          <h2 className="m-0 text-xl font-medium">Automatic push rules</h2>
          {rules.map((r, i) => (
            <AutoPushCard key={`${r.url}-${i}`} rule={r} />
          ))}
        </section>
      )}
    </main>
  );
}

function AutoPushCard({ rule }: { rule: AutoPushRule }) {
  return (
    <div className="flex flex-col gap-2 rounded-lg bg-surface-container-low px-5 py-4 text-sm">
      <div className="flex flex-wrap items-center gap-2">
        <span className="font-semibold">{rule.streams.join(', ')}</span>
        <span className="ms text-base text-on-surface-variant" aria-hidden="true">
          arrow_forward
        </span>
        <span className="num min-w-0 truncate text-on-surface-variant" title={rule.url}>
          {rule.url}
        </span>
        <StatusChip tone="neutral" icon="cast" label={rule.kind.toUpperCase()} />
      </div>
      {rule.active.length === 0 && rule.skipped.length === 0 && (
        <div className="text-on-surface-variant">No live stream matches right now.</div>
      )}
      {rule.active.map((a) => (
        <div key={`a-${a.stream}`} className="flex items-center gap-2">
          <StatusChip tone="primary" icon="radio_button_checked" label="Pushing" />
          <span className="font-semibold">{a.stream}</span>
          <span className="num min-w-0 truncate text-on-surface-variant" title={a.url}>
            {a.url}
          </span>
        </div>
      ))}
      {rule.skipped.map((k) => (
        <div key={`s-${k.stream}`} className="flex items-center gap-2">
          <StatusChip tone="neutral" icon="block" label="Skipped" />
          <span className="font-semibold">{k.stream}</span>
          <span className="min-w-0 truncate text-xs text-on-surface-variant" title={k.reason}>
            {k.reason}
          </span>
        </div>
      ))}
    </div>
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
