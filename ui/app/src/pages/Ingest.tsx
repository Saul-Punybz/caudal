import { listPulls } from '../api';
import { usePolling } from '../hooks/usePolling';
import { ConfigTable } from '../components/ConfigTable';
import { EmptyPanel } from '../components/EmptyPanel';
import { ErrorBanner, ErrorState } from '../components/ErrorState';

/** Configured pull inputs (`GET /api/v1/pulls`): streams Caudal fetches
 * itself from a camera or another server, rather than waiting for a
 * publisher. Read-only here; see docs/PULL-INPUTS.md. */
export function Ingest() {
  const { data: pulls, error, loading } = usePolling(listPulls, 5000);

  return (
    <main className="flex min-w-0 flex-grow flex-col gap-5 py-5 pl-2 pr-6">
      <header>
        <h1 className="display m-0 text-4xl leading-none">Ingest</h1>
        <p className="mt-1 text-sm text-on-surface-variant">Sources this server pulls from.</p>
      </header>

      {loading && !pulls && (
        <div aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </div>
      )}

      {!pulls && error && <ErrorState message={error.message} />}

      {pulls && (
        <>
          {error && <ErrorBanner message={error.message} />}
          {pulls.length === 0 ? (
            <EmptyPanel icon="input" title="No pull inputs configured">
              Add a <code className="num">[[rtsp.pull]]</code>, <code className="num">[[srt.pull]]</code>,{' '}
              <code className="num">[[udp.pull]]</code> or <code className="num">[[http.pull]]</code> section to the
              server config, or <code className="num">POST /api/v1/pulls</code>, and it will show up here.
            </EmptyPanel>
          ) : (
            <ConfigTable label="Pull inputs" entries={pulls} />
          )}
        </>
      )}
    </main>
  );
}
