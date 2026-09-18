import type { ReactNode } from 'react';
import type { Stream } from '../api';
import { formatBitrate } from '../bitrate';
import { formatCount } from '../format';

interface Props {
  streams: Stream[];
  bitrates: Map<string, number | null>;
}

function Card({ icon, label, value }: { icon: string; label: string; value: ReactNode }) {
  return (
    <div className="flex flex-col gap-2 rounded-lg bg-surface-container p-5">
      <div className="flex items-center gap-2 text-sm font-medium text-on-surface-variant">
        <span className="ms text-xl" aria-hidden="true">
          {icon}
        </span>
        {label}
      </div>
      <div className="num text-4xl font-medium leading-none">{value}</div>
    </div>
  );
}

/** Totals computed only from what /api/v1/streams reports — no egress,
 * no uptime, no fake history sparkline. */
export function Totals({ streams, bitrates }: Props) {
  const liveCount = streams.length;
  const totalViewers = streams.reduce((sum, s) => sum + s.stats.viewers, 0);

  let knownBitrate = 0;
  let anyKnown = false;
  for (const bps of bitrates.values()) {
    if (bps !== null) {
      knownBitrate += bps;
      anyKnown = true;
    }
  }

  return (
    <section aria-label="Totals" className="grid grid-cols-2 gap-4 lg:grid-cols-3">
      <Card icon="sensors" label="Live streams" value={formatCount(liveCount)} />
      <Card icon="group" label="Viewers" value={formatCount(totalViewers)} />
      <Card
        icon="download"
        label="Total ingest"
        value={anyKnown || liveCount === 0 ? formatBitrate(anyKnown ? knownBitrate : 0) : '…'}
      />
    </section>
  );
}
