// Shared helpers for exposing hls.js instance and reading metrics.
// Used by both play.spec.ts and steady.spec.ts.

import { type Page } from "@playwright/test";

export interface Metrics {
  browser: string;
  readyState: number;
  videoWidth: number;
  videoHeight: number;
  currentTimeStart?: number;
  currentTimeEnd?: number;
  currentTimeDelta?: number;
  liveEdgeDistanceSec: number | null;
  ingestToGlassSec: number | null;
  fatalHlsError: string | null;
  hlsInstanceExposed: boolean;
  engine: string | null;
  decodedFrameDelta?: number | null;
  decodedFrameSource?: DecodedFrames["source"];
}

/**
 * Playwright can't reach into the page's IIFE to grab its private `hls`
 * variable — play.html never assigns it to `window` (see the report: that
 * would be the one line worth adding there). Instead of touching the page,
 * this proxies the `Hls` global *before* play.html's own script runs: when
 * the page does `new Hls(...)`, the proxy's `construct` trap stashes the
 * real instance on `window.__hlsInstance` and returns it unchanged. Static
 * access (`Hls.Events`, `Hls.isSupported()`, `Hls.version`) still forwards
 * to the real class through the Proxy's default trap, so play.html behaves
 * exactly as it does for a real visitor.
 */
export async function exposeHlsInstance(page: Page): Promise<void> {
  await page.addInitScript(() => {
    let real: any;
    Object.defineProperty(window, "Hls", {
      configurable: true,
      get() {
        return real;
      },
      set(ctor) {
        real = new Proxy(ctor, {
          construct(target, args): object {
            const instance = Reflect.construct(target, args) as object;
            (window as any).__hlsInstance = instance;
            (window as any).__hlsCount = ((window as any).__hlsCount ?? 0) + 1;
            return instance;
          },
        });
      },
    });
  });
}

export interface DecodedFrames {
  /** Frames actually decoded and painted, or null when the browser exposes neither signal. */
  count: number | null;
  source: "getVideoPlaybackQuality" | "webkitDecodedFrameCount" | null;
}

/**
 * A real decoded-frame counter, independent of `currentTime`. `currentTime`
 * alone is not proof of decoding: `play.html`'s live-catch-up guard
 * (`keepUpWithLive`, a forward `video.currentTime = hls.liveSyncPosition`
 * seek fired when the player has been stranded outside hls.js' catch-up
 * range for 2s) can advance `currentTime` by seconds in one tick, which
 * would make a `currentTimeDelta` assertion pass on a seek rather than on
 * real decoding (TEST-AUDIT gap 12). `getVideoPlaybackQuality().totalVideoFrames`
 * (Chromium, Firefox) or the legacy `webkitDecodedFrameCount` (WebKit
 * fallback) count frames the decoder actually produced, so a seek cannot
 * inflate them — only continued playback does.
 *
 * Returns `{ count: null, source: null }` when the browser exposes neither
 * API; callers must skip that assertion cleanly (log + annotate) rather
 * than silently treating null as a pass.
 */
export async function readDecodedFrames(page: Page): Promise<DecodedFrames> {
  return page.evaluate(() => {
    const video = document.getElementById("v") as HTMLVideoElement & {
      webkitDecodedFrameCount?: number;
    };
    if (typeof video.getVideoPlaybackQuality === "function") {
      const q = video.getVideoPlaybackQuality();
      if (q && typeof q.totalVideoFrames === "number" && isFinite(q.totalVideoFrames)) {
        return { count: q.totalVideoFrames, source: "getVideoPlaybackQuality" as const };
      }
    }
    if (typeof video.webkitDecodedFrameCount === "number" && isFinite(video.webkitDecodedFrameCount)) {
      return { count: video.webkitDecodedFrameCount, source: "webkitDecodedFrameCount" as const };
    }
    return { count: null, source: null };
  });
}

/**
 * The frame counter is NOT monotonic on every engine, so subtracting two
 * endpoint samples is wrong. WebKit resets `totalVideoFrames` when the media
 * pipeline re-initialises (hls.js removing and re-appending buffer ranges is
 * enough), which produced negative deltas on CI: -57 frames over 3s in
 * play.spec and -10 over the ~20s steady window, while Chromium and Firefox
 * reported 90 and 601 on the same run. A negative delta is not a stall, it is
 * a counter that went back to zero.
 *
 * So sample inside the page on a short interval and accumulate only the
 * forward movement: each tick adds `cur - prev` when the counter advanced,
 * and on a reset (`cur < prev`) it adds `cur` and re-baselines, losing at
 * most one tick's frames instead of the whole measurement. `resets` is
 * reported so a suspiciously resetting engine stays visible rather than
 * silently passing.
 */
export interface DecodedFrameCount extends DecodedFrames {
  /** How many times the counter went backwards during the window. */
  resets: number;
}

export async function startDecodedFrameCounter(page: Page): Promise<void> {
  await page.evaluate(() => {
    const w = window as any;
    if (w.__frameCounter?.timer) clearInterval(w.__frameCounter.timer);
    const read = (): { count: number | null; source: DecodedFrames["source"] } => {
      const video = document.getElementById("v") as HTMLVideoElement & {
        webkitDecodedFrameCount?: number;
      };
      if (!video) return { count: null, source: null };
      if (typeof video.getVideoPlaybackQuality === "function") {
        const q = video.getVideoPlaybackQuality();
        if (q && typeof q.totalVideoFrames === "number" && isFinite(q.totalVideoFrames)) {
          return { count: q.totalVideoFrames, source: "getVideoPlaybackQuality" };
        }
      }
      if (typeof video.webkitDecodedFrameCount === "number" && isFinite(video.webkitDecodedFrameCount)) {
        return { count: video.webkitDecodedFrameCount, source: "webkitDecodedFrameCount" };
      }
      return { count: null, source: null };
    };
    const first = read();
    const state = {
      total: 0,
      resets: 0,
      prev: first.count,
      source: first.source,
      timer: 0 as unknown as ReturnType<typeof setInterval>,
    };
    state.timer = setInterval(() => {
      const { count, source } = read();
      if (count === null) return;
      if (source) state.source = source;
      if (state.prev === null) {
        state.prev = count;
        return;
      }
      if (count >= state.prev) {
        state.total += count - state.prev;
      } else {
        state.resets += 1;
        state.total += count;
      }
      state.prev = count;
    }, 100);
    w.__frameCounter = state;
  });
}

export async function stopDecodedFrameCounter(page: Page): Promise<DecodedFrameCount> {
  return page.evaluate(() => {
    const state = (window as any).__frameCounter;
    if (!state) return { count: null, source: null, resets: 0 };
    clearInterval(state.timer);
    return {
      count: state.source === null ? null : state.total,
      source: state.source,
      resets: state.resets,
    };
  });
}

export async function readMetrics(page: Page, browserName: string): Promise<Omit<Metrics, "currentTimeStart" | "currentTimeEnd" | "currentTimeDelta">> {
  return page.evaluate((browserName) => {
    const video = document.getElementById("v") as HTMLVideoElement;
    const hls = (window as any).__hlsInstance;
    let liveEdgeDistanceSec: number | null = null;
    let ingestToGlassSec: number | null = null;

    const videoAny = video as HTMLVideoElement & { getStartDate?: () => Date };

    if (hls) {
      // Chromium / hls.js path, exactly as specified in the brief.
      if (typeof hls.latency === "number" && isFinite(hls.latency)) {
        liveEdgeDistanceSec = hls.latency;
      }
      if (hls.playingDate) {
        ingestToGlassSec = (Date.now() - hls.playingDate.getTime()) / 1000;
      }
    } else if (typeof videoAny.getStartDate === "function") {
      // WebKit / native HLS path.
      try {
        const seekable = video.seekable;
        if (seekable && seekable.length > 0) {
          liveEdgeDistanceSec = seekable.end(seekable.length - 1) - video.currentTime;
        }
        const start = videoAny.getStartDate().getTime();
        if (!isNaN(start)) {
          ingestToGlassSec = (Date.now() - start - video.currentTime * 1000) / 1000;
        }
      } catch {
        // getStartDate()/seekable can throw before the first segment loads.
      }
    }

    return {
      browser: browserName,
      readyState: video.readyState,
      videoWidth: video.videoWidth,
      videoHeight: video.videoHeight,
      liveEdgeDistanceSec,
      ingestToGlassSec,
      fatalHlsError: (window as any).__fatalHlsError ?? null,
      hlsInstanceExposed: !!hls,
      engine: document.getElementById("engine")?.textContent ?? null,
    };
  }, browserName);
}
