import { useEffect, useState } from 'react';
import { thumbnailUrl } from '../api';

interface Props {
  stream: string;
  /** Requested picture width in pixels (the server caps it at the source's). */
  width?: number;
  /** How often to fetch a new picture. */
  refreshMs?: number;
  className?: string;
}

/** The stream's newest keyframe as a JPEG, refreshed every few seconds.
 *
 * Each refresh is preloaded off-screen and only swapped in once it has
 * decoded, so the picture never flashes empty. A failed refresh keeps the
 * last good picture; until the first one arrives a quiet camera icon
 * stands in. 409 (no video track) and 503 (no keyframe yet, ffmpeg missing
 * or busy) are normal answers from the server and are never shown as an
 * error. Refreshing pauses while the tab is hidden. */
export function Thumbnail({ stream, width = 640, refreshMs = 5000, className = '' }: Props) {
  const [src, setSrc] = useState<string | null>(null);

  useEffect(() => {
    setSrc(null);
    let cancelled = false;
    let pending: HTMLImageElement | null = null;

    function load() {
      if (typeof document !== 'undefined' && document.visibilityState === 'hidden') return;
      const url = thumbnailUrl(stream, width, Date.now());
      const img = new Image();
      pending = img;
      img.onload = () => {
        if (!cancelled && pending === img) setSrc(url);
      };
      // onerror: keep whatever is on screen (last picture or the icon).
      img.src = url;
    }

    load();
    const timer = setInterval(load, refreshMs);
    return () => {
      cancelled = true;
      clearInterval(timer);
      if (pending) pending.onload = null;
    };
  }, [stream, width, refreshMs]);

  return (
    <div
      className={`relative flex items-center justify-center overflow-hidden bg-[var(--md-sys-color-surface-container-lowest)] ${className}`}
    >
      {src ? (
        <img src={src} alt={`Latest frame of ${stream}`} className="h-full w-full object-contain" />
      ) : (
        <span className="ms text-5xl text-on-surface-variant" aria-hidden="true">
          videocam
        </span>
      )}
    </div>
  );
}
