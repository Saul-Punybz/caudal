import { useRef } from 'react';
import type { Stream } from '../api';
import { bitrateFromDelta, type Sample } from '../bitrate';

/**
 * Tracks `bytes_in` across polls (keyed by stream name) and returns the
 * current ingest bitrate for each stream in `streams`, in bits/second.
 * `null` for a stream until its second sample arrives.
 */
export function useBitrates(streams: Stream[] | null): Map<string, number | null> {
  const previous = useRef<Map<string, Sample>>(new Map());

  const result = new Map<string, number | null>();
  if (!streams) return result;

  const now = Date.now();
  const seen = new Set<string>();
  for (const s of streams) {
    seen.add(s.name);
    const current: Sample = { bytesIn: s.stats.bytes_in, atMs: now };
    const prev = previous.current.get(s.name);
    result.set(s.name, bitrateFromDelta(prev, current));
    previous.current.set(s.name, current);
  }
  // Drop streams that disappeared so a name reused later starts fresh.
  for (const name of previous.current.keys()) {
    if (!seen.has(name)) previous.current.delete(name);
  }

  return result;
}
