import type Hls from 'hls.js';
import { useEffect, useRef, useState } from 'react';

interface Props {
  streamName: string;
  /** Called whenever the measured "seconds behind live" changes. */
  onLatency: (seconds: number | null) => void;
}

/** hls.js in low-latency mode, with a native-HLS fallback for Safari.
 * Muted autoplay so browsers don't block it. hls.js (~150KB gzipped) is
 * dynamically imported so the Overview page never has to load it. */
export function Player({ streamName, onLatency }: Props) {
  const videoRef = useRef<HTMLVideoElement>(null);
  const [playbackError, setPlaybackError] = useState<string | null>(null);

  useEffect(() => {
    const video = videoRef.current;
    if (!video) return;
    setPlaybackError(null);
    const src = `/hls/${encodeURIComponent(streamName)}/master.m3u8`;

    let hls: Hls | null = null;
    let latencyTimer: ReturnType<typeof setInterval> | undefined;
    let cancelled = false;

    async function setup() {
      const { default: HlsCtor } = await import('hls.js');
      if (cancelled || !video) return;

      if (HlsCtor.isSupported()) {
        // Catch up to the live target at up to 1.5x instead of staying wherever
        // playback started (see crates/caudal-hls/static/play.html).
        hls = new HlsCtor({ lowLatencyMode: true, maxLiveSyncPlaybackRate: 1.5 });
        hls.loadSource(src);
        hls.attachMedia(video);
        hls.on(HlsCtor.Events.ERROR, (_evt, data) => {
          if (data.fatal) {
            setPlaybackError(`${data.type}: ${data.details}`);
          }
        });
        latencyTimer = setInterval(() => {
          const latency = hls?.latency;
          onLatency(typeof latency === 'number' && Number.isFinite(latency) ? latency : null);
        }, 1000);
      } else if (video.canPlayType('application/vnd.apple.mpegurl')) {
        // Safari plays HLS natively.
        video.src = src;
        latencyTimer = setInterval(() => {
          const seekable = video.seekable;
          if (seekable.length > 0) {
            onLatency(seekable.end(seekable.length - 1) - video.currentTime);
          } else {
            onLatency(null);
          }
        }, 1000);
      } else {
        setPlaybackError('This browser has no HLS support (native or hls.js).');
      }
    }
    void setup();

    return () => {
      cancelled = true;
      if (latencyTimer) clearInterval(latencyTimer);
      hls?.destroy();
      onLatency(null);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [streamName]);

  return (
    <div className="relative aspect-video w-full overflow-hidden rounded-lg bg-[#0b0d1b]">
      <video
        ref={videoRef}
        muted
        autoPlay
        playsInline
        controls
        className="h-full w-full"
        aria-label={`Live preview of ${streamName}`}
      />
      {playbackError && (
        <div
          role="alert"
          className="absolute inset-0 flex items-center justify-center bg-black/70 p-4 text-center text-sm text-white"
        >
          Playback error: {playbackError}
        </div>
      )}
    </div>
  );
}
