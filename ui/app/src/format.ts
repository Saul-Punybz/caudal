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
