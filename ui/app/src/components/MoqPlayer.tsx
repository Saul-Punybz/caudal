import { useEffect, useRef, useState } from 'react';
import type MoqWatchElement from '@moq/watch/element';
import { buildMoqUrl, hexFingerprintToBytes, withTimeout } from '../moq';

interface Props {
  /** The relay's own base URL, from `GET /moq/fingerprint`'s `url` field. */
  relayUrl: string;
  /** The broadcast name, fixed to be exactly the stream name (STATUS.md "Batch 5"). */
  broadcastName: string;
  /** Lowercase hex SHA-256 of the relay's certificate, or null for a real cert. */
  fingerprint: string | null;
  /** From the page's `?token=`, if any. Forwarded as `?jwt=` on the relay URL. */
  token?: string;
  /** Called with the player's own jitter-buffer readout (`<moq-watch>.jitter`,
   * milliseconds), or null while unknown. */
  onLatencyMs: (ms: number | null) => void;
}

type BroadcastStatus = 'offline' | 'loading' | 'live';

/** How long the lazily loaded `@moq/watch` module may take before the
 * player says so instead of sitting on "Connecting…". */
const LOAD_TIMEOUT_MS = 15_000;

/** One `[moq-ui]` debug line per setup step, so a player that never
 * connects says which step it stopped at (tests/browser/moq.spec.ts
 * forwards the page console). */
function step(msg: string) {
  console.debug(`[moq-ui] ${msg}`);
}

/** Media over QUIC playback via moq-dev's official `@moq/watch` player.
 * `<moq-watch>` renders to a nested `<canvas>` (there is no `MediaStream` or
 * `<video>` involved — WebCodecs decodes straight to frames). Lazily
 * imported, like hls.js, so the Overview bundle never pays for it. */
export function MoqPlayer({ relayUrl, broadcastName, fingerprint, token, onLatencyMs }: Props) {
  const mountRef = useRef<HTMLDivElement>(null);
  const [playbackError, setPlaybackError] = useState<string | null>(null);
  const [status, setStatus] = useState<BroadcastStatus>('loading');
  // Bumped by the Retry button to run setup again from scratch.
  const [attempt, setAttempt] = useState(0);

  useEffect(() => {
    let cancelled = false;
    let el: MoqWatchElement | null = null;
    let statsTimer: ReturnType<typeof setInterval> | undefined;
    let unsubscribeStatus: (() => void) | undefined;
    setPlaybackError(null);
    setStatus('loading');
    onLatencyMs(null);

    if (typeof window === 'undefined' || !('WebTransport' in window)) {
      step('no WebTransport in this browser');
      setPlaybackError('This browser has no WebTransport support.');
      return;
    }

    async function setup() {
      let url: URL;
      let certHashes: Uint8Array<ArrayBuffer> | null = null;
      try {
        url = buildMoqUrl(relayUrl, token);
        certHashes = fingerprint ? hexFingerprintToBytes(fingerprint) : null;
      } catch (err) {
        step(`bad relay URL or fingerprint: ${(err as Error).message}`);
        setPlaybackError((err as Error).message);
        return;
      }

      // Side-effectful: registers the <moq-watch> custom element.
      step(`loading player module (attempt ${attempt + 1})`);
      let MoqWatch: typeof MoqWatchElement;
      try {
        ({ default: MoqWatch } = await withTimeout(
          import('@moq/watch/element'),
          LOAD_TIMEOUT_MS,
          'loading the MoQ player',
        ));
      } catch (err) {
        step(`player module failed: ${(err as Error).message}`);
        if (!cancelled) setPlaybackError((err as Error).message);
        return;
      }
      if (cancelled || !mountRef.current) {
        step(`player module loaded after unmount (cancelled=${cancelled}); not mounting`);
        return;
      }
      step('player module loaded');

      const watch = new MoqWatch();
      if (certHashes) {
        watch.connection.webtransport = {
          serverCertificateHashes: [{ algorithm: 'sha-256', value: certHashes }],
        };
      }
      watch.catalogFormat = 'hang';
      watch.name = broadcastName;

      const canvas = document.createElement('canvas');
      canvas.className = 'h-full w-full';
      canvas.setAttribute('aria-label', `Live preview of ${broadcastName} over MoQ`);
      watch.appendChild(canvas);

      unsubscribeStatus = watch.broadcast.out.status.subscribe((s) => {
        step(`broadcast status ${s}`);
        if (!cancelled) setStatus(s);
      });

      // Setting `url` last is what starts the connection attempt (via
      // connectedCallback once appended below), so every option above is
      // already in place before the first attempt.
      mountRef.current.appendChild(watch);
      watch.url = url;
      el = watch;
      step(`<moq-watch> mounted, url=${url.origin}${url.pathname} hashes=${certHashes ? 1 : 0} name=${broadcastName}`);

      statsTimer = setInterval(() => {
        if (cancelled) return;
        const ms = el?.jitter;
        onLatencyMs(typeof ms === 'number' && Number.isFinite(ms) ? ms : null);
      }, 1000);
    }

    void setup().catch((err: unknown) => {
      step(`setup threw: ${(err as Error)?.message ?? err}`);
      if (!cancelled) setPlaybackError((err as Error)?.message ?? String(err));
    });

    return () => {
      cancelled = true;
      if (statsTimer) clearInterval(statsTimer);
      unsubscribeStatus?.();
      // Removing it from the DOM fires disconnectedCallback, which disables
      // the connection the same way unmounting a <video> stops playback.
      if (el?.parentNode) el.parentNode.removeChild(el);
      onLatencyMs(null);
    };
  }, [relayUrl, broadcastName, fingerprint, token, onLatencyMs, attempt]);

  return (
    <div className="relative aspect-video w-full overflow-hidden rounded-lg bg-[#0b0d1b]">
      <div ref={mountRef} className="h-full w-full" />
      {!playbackError && status !== 'live' && (
        <div
          aria-live="polite"
          className="pointer-events-none absolute inset-0 flex items-center justify-center text-sm text-on-surface-variant"
        >
          {status === 'offline' ? 'Broadcast offline' : 'Connecting over MoQ…'}
        </div>
      )}
      {playbackError && (
        <div
          role="alert"
          className="absolute inset-0 flex flex-col items-center justify-center gap-3 bg-black/70 p-4 text-center text-sm text-white"
        >
          <span>MoQ error: {playbackError}</span>
          <button
            type="button"
            onClick={() => setAttempt((n) => n + 1)}
            className="state-layer h-10 rounded-full bg-primary px-4 text-sm font-semibold text-on-primary"
          >
            Retry
          </button>
        </div>
      )}
    </div>
  );
}
