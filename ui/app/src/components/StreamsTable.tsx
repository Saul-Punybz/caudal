import type { Stream } from '../api';
import { formatBitrate } from '../bitrate';
import { formatCount, formatSeconds, summarizeTracks } from '../format';
import { LiveBadge } from './LiveBadge';

interface Props {
  streams: Stream[];
  bitrates: Map<string, number | null>;
  selected: string | null;
  onSelect: (name: string) => void;
}

/** Every stream returned by the API is, by definition, currently
 * publishing — the API has no idle/fault status, so every row is LIVE. */
export function StreamsTable({ streams, bitrates, selected, onSelect }: Props) {
  return (
    <div role="table" aria-label="Streams" className="flex flex-col overflow-auto">
      <div
        role="row"
        className="grid grid-cols-[1.6fr_0.7fr_2fr_0.7fr_0.9fr_0.9fr] gap-3 border-b border-outline-variant px-5 py-2 text-xs font-medium uppercase tracking-wide text-on-surface-variant"
      >
        <span role="columnheader">Stream</span>
        <span role="columnheader">Status</span>
        <span role="columnheader">Tracks</span>
        <span role="columnheader" className="text-right">
          Viewers
        </span>
        <span role="columnheader" className="text-right">
          Bitrate
        </span>
        <span role="columnheader" className="text-right">
          Buffered
        </span>
      </div>
      {streams.map((s) => {
        const isSelected = s.name === selected;
        return (
          <div
            key={s.name}
            role="row"
            tabIndex={0}
            aria-selected={isSelected}
            onClick={() => onSelect(s.name)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' || e.key === ' ') {
                e.preventDefault();
                onSelect(s.name);
              }
            }}
            className={`row grid cursor-pointer grid-cols-[1.6fr_0.7fr_2fr_0.7fr_0.9fr_0.9fr] items-center gap-3 border-b border-outline-variant px-5 text-sm ${
              isSelected ? 'bg-secondary-container' : ''
            }`}
            style={{ minHeight: 56 }}
          >
            <span role="cell" className="font-semibold">
              {s.name}
            </span>
            <span role="cell">
              <LiveBadge live />
            </span>
            <span role="cell" className="num text-sm text-on-surface-variant">
              {summarizeTracks(s.tracks)}
            </span>
            <span role="cell" className="num text-right">
              {formatCount(s.stats.viewers)}
            </span>
            <span role="cell" className="num text-right">
              {formatBitrate(bitrates.get(s.name) ?? null)}
            </span>
            <span role="cell" className="num text-right">
              {formatSeconds(s.stats.buffered_ms)}
            </span>
          </div>
        );
      })}
    </div>
  );
}
