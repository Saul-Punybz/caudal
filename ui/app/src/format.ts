import type { Track } from './api';

export function formatCount(n: number): string {
  return n.toLocaleString('en-US');
}

export function formatSeconds(ms: number): string {
  return `${(ms / 1000).toFixed(1)} s`;
}

/** "H.264 1280x720 30p · AAC 48 kHz" style summary, built only from fields the API sends. */
export function summarizeTrack(t: Track): string {
  const codec = t.codec.toUpperCase();
  if (t.kind === 'video') {
    const dims = t.width && t.height ? `${t.width}x${t.height}` : null;
    const fps = t.fps ? `${Math.round(t.fps)}p` : null;
    return [codec, dims, fps].filter(Boolean).join(' ');
  }
  if (t.kind === 'audio') {
    const rate = t.sample_rate ? `${(t.sample_rate / 1000).toFixed(0)} kHz` : null;
    const ch = t.channels ? (t.channels === 1 ? 'mono' : t.channels === 2 ? 'stereo' : `${t.channels}ch`) : null;
    return [codec, rate, ch].filter(Boolean).join(' ');
  }
  return codec;
}

export function summarizeTracks(tracks: Track[]): string {
  if (tracks.length === 0) return '—';
  return tracks.map(summarizeTrack).join(' · ');
}

/** "512 B" / "480 KB" / "1.2 GB" — binary (1024) units, matching how disks
 * report size. */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes)) return '0 B';
  if (bytes < 1024) return `${Math.max(0, Math.round(bytes))} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let value = bytes / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i++;
  }
  return `${value.toFixed(value >= 100 ? 0 : 1)} ${units[i]}`;
}

/** "45s" / "12:34" / "1:02:03" — for a count of whole seconds. */
export function formatDurationSecs(totalSeconds: number): string {
  const s = Math.max(0, Math.floor(totalSeconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  if (h > 0) return `${h}:${String(m).padStart(2, '0')}:${String(sec).padStart(2, '0')}`;
  if (m > 0) return `${m}:${String(sec).padStart(2, '0')}`;
  return `${sec}s`;
}
