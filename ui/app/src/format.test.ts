import { describe, expect, it } from 'vitest';
import { formatBytes, formatDurationSecs } from './format';

describe('formatBytes', () => {
  it('formats sub-KB counts as whole bytes', () => {
    expect(formatBytes(0)).toBe('0 B');
    expect(formatBytes(512)).toBe('512 B');
    expect(formatBytes(1023)).toBe('1023 B');
  });

  it('formats KB/MB/GB with one decimal under 100', () => {
    expect(formatBytes(1024)).toBe('1.0 KB');
    expect(formatBytes(1536)).toBe('1.5 KB');
    expect(formatBytes(12 * 1024 * 1024)).toBe('12.0 MB');
    expect(formatBytes(1.2 * 1024 * 1024 * 1024)).toBe('1.2 GB');
  });

  it('drops the decimal at 100 units and above', () => {
    expect(formatBytes(150 * 1024)).toBe('150 KB');
  });

  it('clamps negative or non-finite input to 0 B', () => {
    expect(formatBytes(-5)).toBe('0 B');
    expect(formatBytes(NaN)).toBe('0 B');
  });
});

describe('formatDurationSecs', () => {
  it('formats sub-minute durations as seconds', () => {
    expect(formatDurationSecs(0)).toBe('0s');
    expect(formatDurationSecs(45)).toBe('45s');
  });

  it('formats sub-hour durations as m:ss', () => {
    expect(formatDurationSecs(75)).toBe('1:15');
    expect(formatDurationSecs(754)).toBe('12:34');
  });

  it('formats hour-plus durations as h:mm:ss', () => {
    expect(formatDurationSecs(3723)).toBe('1:02:03');
  });

  it('floors fractional seconds and clamps negatives to 0', () => {
    expect(formatDurationSecs(45.9)).toBe('45s');
    expect(formatDurationSecs(-5)).toBe('0s');
  });
});
