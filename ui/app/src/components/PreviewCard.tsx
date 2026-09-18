import { Link } from 'react-router-dom';
import type { Stream } from '../api';
import { formatBitrate } from '../bitrate';
import { formatCount, formatSeconds, summarizeTracks } from '../format';
import { LiveBadge } from './LiveBadge';

interface Props {
  stream: Stream;
  bitrate: number | null;
}

/** Right-hand summary for the row selected in the streams table. No
 * thumbnail: the API doesn't provide one, so a placeholder icon stands in
 * rather than a fake frame. The real preview lives on the detail page's
 * hls.js player. */
export function PreviewCard({ stream, bitrate }: Props) {
  return (
    <aside aria-label="Selected stream" className="flex w-full flex-col gap-4 lg:w-[360px]">
      <div className="flex flex-col overflow-hidden rounded-lg bg-surface-container">
        <div className="relative flex h-40 items-center justify-center bg-[var(--md-sys-color-surface-container-lowest)]">
          <span className="ms text-5xl text-on-surface-variant" aria-hidden="true">
            videocam
          </span>
          <LiveBadge live className="absolute left-3 top-3" />
        </div>
        <div className="flex flex-col gap-3 p-5">
          <div className="flex items-center gap-2">
            <h2 className="m-0 text-xl font-medium">{stream.name}</h2>
            <div className="flex-grow" />
            <Link
              to={`/streams/${encodeURIComponent(stream.name)}`}
              className="state-layer flex items-center gap-1 rounded-full px-3 py-2 text-sm font-semibold no-underline"
              style={{ minHeight: 40 }}
            >
              Open
              <span className="ms text-lg" aria-hidden="true">
                arrow_forward
              </span>
            </Link>
          </div>
          <div className="num text-sm text-on-surface-variant">
            {summarizeTracks(stream.tracks)}
          </div>
          <div className="grid grid-cols-3 gap-2">
            <Stat label="Viewers" value={formatCount(stream.stats.viewers)} />
            <Stat label="Bitrate" value={formatBitrate(bitrate)} />
            <Stat label="Buffered" value={formatSeconds(stream.stats.buffered_ms)} />
          </div>
        </div>
      </div>
    </aside>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-md bg-surface-container-high px-3 py-2.5">
      <div className="text-xs text-on-surface-variant">{label}</div>
      <div className="num text-xl font-medium">{value}</div>
    </div>
  );
}
