import type { ConfigEntry } from '../api';

/** Read-only table of configured pulls or pushes (`stream`, `url`,
 * `section`), the shape `GET /api/v1/pulls` and `/api/v1/pushes` share.
 * URLs arrive with their secrets already masked by the server. */
export function ConfigTable({ label, entries }: { label: string; entries: ConfigEntry[] }) {
  return (
    <div role="table" aria-label={label} className="flex flex-col overflow-hidden rounded-lg bg-surface-container-low">
      <div
        role="row"
        className="grid grid-cols-[1fr_2.2fr_1fr] gap-3 border-b border-outline-variant px-5 py-2 text-xs font-medium uppercase tracking-wide text-on-surface-variant"
      >
        <span role="columnheader">Stream</span>
        <span role="columnheader">URL</span>
        <span role="columnheader">Config section</span>
      </div>
      {entries.map((e, i) => (
        <div
          key={`${e.section}-${e.stream}-${e.url}-${i}`}
          role="row"
          className="grid grid-cols-[1fr_2.2fr_1fr] items-center gap-3 border-b border-outline-variant px-5 text-sm last:border-b-0"
          style={{ minHeight: 48 }}
        >
          <span role="cell" className="truncate font-semibold" title={e.stream}>
            {e.stream}
          </span>
          <span role="cell" className="num truncate text-on-surface-variant" title={e.url}>
            {e.url}
          </span>
          <span role="cell" className="num truncate text-xs text-on-surface-variant">
            {e.section}
          </span>
        </div>
      ))}
    </div>
  );
}
