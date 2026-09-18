import { useMemo, useState } from 'react';
import { listStreams } from '../api';
import { useBitrates } from '../hooks/useBitrates';
import { usePolling } from '../hooks/usePolling';
import { ThemeToggle } from '../components/ThemeToggle';
import { Totals } from '../components/Totals';
import { StreamsTable } from '../components/StreamsTable';
import { PreviewCard } from '../components/PreviewCard';
import { EmptyState } from '../components/EmptyState';
import { ErrorBanner, ErrorState } from '../components/ErrorState';

export function Overview() {
  const { data: streams, error, loading } = usePolling(listStreams, 1000);
  const bitrates = useBitrates(streams);
  const [selected, setSelected] = useState<string | null>(null);
  const [query, setQuery] = useState('');

  const filtered = useMemo(() => {
    if (!streams) return [];
    const q = query.trim().toLowerCase();
    if (!q) return streams;
    return streams.filter((s) => s.name.toLowerCase().includes(q));
  }, [streams, query]);

  const selectedStream = useMemo(() => {
    if (filtered.length === 0) return null;
    return filtered.find((s) => s.name === selected) ?? filtered[0];
  }, [filtered, selected]);

  return (
    <main className="flex min-w-0 flex-grow flex-col gap-5 py-5 pl-2 pr-6">
      <header className="flex flex-wrap items-center gap-4">
        <h1 className="display m-0 text-4xl leading-none">Overview</h1>
        <div className="flex-grow" />
        <label className="flex h-12 w-full max-w-xs items-center gap-2 rounded-full bg-surface-container-high px-4 text-on-surface-variant sm:w-80">
          <span className="ms" aria-hidden="true">
            search
          </span>
          <input
            type="search"
            placeholder="Search streams"
            aria-label="Search streams"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            className="min-w-0 flex-grow border-0 bg-transparent text-base text-on-surface outline-none placeholder:text-on-surface-variant"
          />
        </label>
        <ThemeToggle />
      </header>

      {loading && !streams && (
        <div aria-live="polite" className="text-sm text-on-surface-variant">
          Loading…
        </div>
      )}

      {!streams && error && <ErrorState message={error.message} />}

      {streams && (
        <>
          {error && <ErrorBanner message={error.message} />}
          <Totals streams={streams} bitrates={bitrates} />
          <div className="flex min-h-0 flex-grow flex-col gap-4 lg:flex-row">
            <section
              aria-label="Streams"
              className="flex min-w-0 flex-grow flex-col overflow-hidden rounded-lg bg-surface-container-low"
            >
              <div className="flex items-center gap-3 px-5 pb-3 pt-4">
                <h2 className="m-0 text-xl font-medium">Streams</h2>
              </div>
              {streams.length === 0 ? (
                <EmptyState />
              ) : filtered.length === 0 ? (
                <p className="px-5 pb-5 text-sm text-on-surface-variant">
                  No stream matches "{query}".
                </p>
              ) : (
                <StreamsTable
                  streams={filtered}
                  bitrates={bitrates}
                  selected={selectedStream?.name ?? null}
                  onSelect={setSelected}
                />
              )}
            </section>
            {selectedStream && (
              <PreviewCard
                stream={selectedStream}
                bitrate={bitrates.get(selectedStream.name) ?? null}
              />
            )}
          </div>
        </>
      )}
    </main>
  );
}
