import { describe, expect, it } from 'vitest';
import { bitrateFromDelta, formatBitrate } from './bitrate';

describe('bitrateFromDelta', () => {
  it('returns null with no previous sample', () => {
    expect(bitrateFromDelta(undefined, { bytesIn: 1000, atMs: 1000 })).toBeNull();
  });

  it('computes bits per second from a byte delta over elapsed time', () => {
    // 125,000 bytes over 1000ms = 1,000,000 bits/s
    const prev = { bytesIn: 0, atMs: 0 };
    const cur = { bytesIn: 125_000, atMs: 1000 };
    expect(bitrateFromDelta(prev, cur)).toBe(1_000_000);
  });

  it('returns null when no time elapsed', () => {
    const prev = { bytesIn: 0, atMs: 1000 };
    const cur = { bytesIn: 500, atMs: 1000 };
    expect(bitrateFromDelta(prev, cur)).toBeNull();
  });

  it('returns null when the counter goes backwards (stream reset)', () => {
    const prev = { bytesIn: 5000, atMs: 0 };
    const cur = { bytesIn: 100, atMs: 1000 };
    expect(bitrateFromDelta(prev, cur)).toBeNull();
  });

  it('handles zero delta (idle stream) as zero bitrate', () => {
    const prev = { bytesIn: 5000, atMs: 0 };
    const cur = { bytesIn: 5000, atMs: 1000 };
    expect(bitrateFromDelta(prev, cur)).toBe(0);
  });
});

describe('formatBitrate', () => {
  it('renders an em dash for null', () => {
    expect(formatBitrate(null)).toBe('—');
  });
  it('renders bits/s below 1000', () => {
    expect(formatBitrate(480)).toBe('480 b/s');
  });
  it('renders kb/s below 1,000,000', () => {
    expect(formatBitrate(480_000)).toBe('480 kb/s');
  });
  it('renders Mb/s at or above 1,000,000', () => {
    expect(formatBitrate(2_400_000)).toBe('2.4 Mb/s');
  });
});
