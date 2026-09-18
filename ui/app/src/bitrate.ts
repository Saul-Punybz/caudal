// Computes live bitrate from two successive polls of `bytes_in`, since the
// API only reports a cumulative byte counter, not a rate.

export interface Sample {
  bytesIn: number;
  atMs: number;
}

/**
 * Bits per second between two samples of a monotonically increasing byte
 * counter. Returns null when the samples can't produce a sane rate: no
 * previous sample yet, no time elapsed, or the counter went backwards
 * (stream restarted/reset).
 */
export function bitrateFromDelta(previous: Sample | undefined, current: Sample): number | null {
  if (!previous) return null;
  const dtMs = current.atMs - previous.atMs;
  if (dtMs <= 0) return null;
  const dBytes = current.bytesIn - previous.bytesIn;
  if (dBytes < 0) return null;
  return (dBytes * 8 * 1000) / dtMs;
}

/** Formats a bits-per-second number as "1.2 Mb/s" / "480 kb/s" / "0 b/s". */
export function formatBitrate(bps: number | null): string {
  if (bps === null) return '—';
  if (bps >= 1_000_000) return `${(bps / 1_000_000).toFixed(1)} Mb/s`;
  if (bps >= 1_000) return `${(bps / 1_000).toFixed(0)} kb/s`;
  return `${Math.round(bps)} b/s`;
}
