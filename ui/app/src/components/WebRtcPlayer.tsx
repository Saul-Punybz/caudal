import { useEffect, useRef, useState } from 'react';
import type { AuthErrorKind } from './Player';
import { WebRtcError, measureJitterBufferMs, startWhep, type WhepSession } from '../webrtc';

interface Props {
  /** Absolute `<origin>/whep/<name>` URL. */
  url: string;
  token?: string;
  /** Called with the estimated playout buffer, in milliseconds (see
   * `measureJitterBufferMs`), or null while unknown. */
  onLatencyMs: (ms: number | null) => void;
  onAuthError: (kind: AuthErrorKind | null) => void;
}

/** Plays a stream over WHEP: negotiate once, attach the resulting
 * `MediaStream` to a <video>, then poll `getStats()` for the jitter-buffer
 * estimate. Muted autoplay for the same reason as the HLS player. */
export function WebRtcPlayer({ url, token, onLatencyMs, onAuthError }: Props) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const [playbackError, setPlaybackError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    let session: WhepSession | null = null;
    let statsTimer: ReturnType<typeof setInterval> | undefined;
    setPlaybackError(null);
    onAuthError(null);

    async function setup() {
      try {
        const s = await startWhep({ url, token });
        if (cancelled) {
          void s.stop();
          return;
        }
        session = s;
        if (videoRef.current) videoRef.current.srcObject = s.stream;
        statsTimer = setInterval(() => {
          void measureJitterBufferMs(s.pc).then((ms) => {
            if (!cancelled) onLatencyMs(ms);
          });
        }, 1000);
      } catch (err) {
        if (cancelled) return;
        if (err instanceof WebRtcError && err.status === 401) {
          onAuthError('missing');
          return;
        }
        if (err instanceof WebRtcError && err.status === 403) {
          onAuthError('refused');
          return;
        }
        setPlaybackError((err as Error).message);
      }
    }
    void setup();

    return () => {
      cancelled = true;
      if (statsTimer) clearInterval(statsTimer);
      if (session) void session.stop();
      onLatencyMs(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [url, token]);

  return (
    <div className="relative aspect-video w-full overflow-hidden rounded-lg bg-[#0b0d1b]">
      <video
        ref={videoRef}
        muted
        autoPlay
        playsInline
        controls
        className="h-full w-full"
        aria-label="Live preview over WebRTC"
      />
      {playbackError && (
        <div
          role="alert"
          className="absolute inset-0 flex items-center justify-center bg-black/70 p-4 text-center text-sm text-white"
        >
          WebRTC error: {playbackError}
        </div>
      )}
    </div>
  );
}
