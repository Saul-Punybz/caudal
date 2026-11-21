import { useState } from 'react';
import { kickSession, listSessions, type ViewerSession } from '../api';
import { usePolling } from '../hooks/usePolling';
import { ErrorBanner } from './ErrorState';
import { formatBytes, formatDurationSecs } from '../format';

/** Who is watching one stream right now (`GET /api/v1/sessions?stream=`),
 * with a button to end a viewer's connection. Mounted only while its tab
 * is open, so it polls only then. This page's own preview player counts
 * as a viewer too (LL-HLS, WHEP or MoQ). */
export function ViewersPanel({ stream }: { stream: string }) {
  const { data: sessions, error, loading } = usePolling(() => listSessions(stream), 2000);
  const [kickError, setKickError] = useState<string | null>(null);

  async function kick(id: number) {
    setKickError(null);
    try {
      await kickSession(id);
    } catch (err) {
      setKickError((err as Error).message);
    }
  }

  return (
    <div role="tabpanel" className="flex flex-col gap-2 p-4">
      {error && <ErrorBanner message={error.message} />}
      {kickError && <ErrorBanner message={kickError} />}
      {loading && !sessions && (
        <p aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </p>
      )}
      {sessions && sessions.length === 0 && <p className="text-sm text-on-surface-variant">No viewers right now.</p>}
      {sessions?.map((s) => <SessionRow key={s.id} session={s} onKick={() => void kick(s.id)} />)}
      {sessions && sessions.length > 0 && (
        <p className="m-0 text-xs text-on-surface-variant">
          A player that reconnects by itself comes back within seconds.
        </p>
      )}
    </div>
  );
}

function SessionRow({ session: s, onKick }: { session: ViewerSession; onKick: () => void }) {
  const net: string[] = [];
  if (s.rtt_ms !== null) net.push(`${s.rtt_ms.toFixed(0)} ms rtt`);
  if (s.loss !== null) net.push(`${(s.loss * 100).toFixed(1)}% loss`);
  return (
    <div className="flex items-center gap-3 rounded-md bg-surface-container-high p-3.5">
      <span className="ms text-on-surface-variant" aria-hidden="true">
        person
      </span>
      <div className="min-w-0 flex-grow">
        <div className="flex flex-wrap items-center gap-2 font-semibold">
          <span className="uppercase">{s.protocol}</span>
          <span className="num truncate text-sm font-normal text-on-surface-variant">{s.ip ?? 'address not reported'}</span>
        </div>
        <div className="num text-sm text-on-surface-variant">
          {formatDurationSecs(s.duration_secs)} · {formatBytes(s.bytes_out)}
          {net.length > 0 ? ` · ${net.join(' · ')}` : ''}
          {s.tags.length > 0 ? ` · ${s.tags.join(', ')}` : ''}
        </div>
      </div>
      <button
        type="button"
        onClick={onKick}
        aria-label={`Disconnect ${s.protocol} viewer ${s.ip ?? s.id}`}
        title="Disconnect this viewer"
        className="state-layer flex flex-shrink-0 items-center justify-center rounded-full border-0 bg-transparent text-on-surface-variant"
        style={{ width: 40, height: 40 }}
      >
        <span className="ms" aria-hidden="true">
          logout
        </span>
      </button>
    </div>
  );
}
