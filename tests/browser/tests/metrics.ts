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
