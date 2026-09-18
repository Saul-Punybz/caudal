// Measures steady-state ingest-to-glass latency after initial startup.
// Unlike play.spec.ts which checks for startup convergence within 15s, this
// test runs long enough to observe the live catch-up behaviour (maxLiveSyncPlaybackRate: 1.5)
// after playback stabilizes. One binary + one ffmpeg publish per file (separate from play.spec.ts).

import { test, expect } from "@playwright/test";
import { writeFile } from "node:fs/promises";
import { join } from "node:path";
import {
  haveFfmpeg,
  startCaudal,
  startFfmpegPublisher,
  ensureResultsDir,
  type CaudalServer,
  type FfmpegPublisher,
} from "./harness";
import { exposeHlsInstance, readMetrics } from "./metrics";

const STREAM_NAME = "steady";

interface SteadyStateMetrics {
  browser: string;
  startupIngestToGlassSec: number | null;
  steadyStateMin: number | null;
  steadyStateMedian: number | null;
  steadyStateMax: number | null;
  validSampleCount: number;
  fatalHlsError: string | null;
  hlsInstanceExposed: boolean;
  engine: string | null;
}

test.describe("Caudal LL-HLS steady-state latency", () => {
  test.skip(!haveFfmpeg(), "ffmpeg is not installed — skipping the whole browser suite (see brief step 2)");

  let server: CaudalServer;
  let publisher: FfmpegPublisher;

  test.beforeAll(async ({ browserName }) => {
    server = await startCaudal({ tls: browserName === "webkit" });
    publisher = startFfmpegPublisher(server.rtmpUrl(STREAM_NAME));
    // Give ffmpeg + the RTMP handshake + the first LL-HLS parts a moment
    // before a browser asks for the playlist.
    const deadline = Date.now() + 15_000;
    let ready = false;
    const baseUrl = browserName === "webkit" ? server.httpsBaseUrl || server.baseUrl : server.baseUrl;
    while (Date.now() < deadline) {
      const oldRejectUnauth = process.env.NODE_TLS_REJECT_UNAUTHORIZED;
      if (browserName === "webkit") {
        process.env.NODE_TLS_REJECT_UNAUTHORIZED = "0";
      }
      try {
        const res = await fetch(`${baseUrl}/hls/${STREAM_NAME}/master.m3u8`).catch(() => null);
        if (res && res.status === 200) {
          ready = true;
          break;
        }
      } finally {
        if (oldRejectUnauth !== undefined) {
          process.env.NODE_TLS_REJECT_UNAUTHORIZED = oldRejectUnauth;
        } else {
          delete process.env.NODE_TLS_REJECT_UNAUTHORIZED;
        }
      }
      await new Promise((r) => setTimeout(r, 250));
    }
    if (!ready) {
      throw new Error(`master.m3u8 for "${STREAM_NAME}" never became available — is ffmpeg actually publishing?`);
    }
  });

  test.afterAll(async () => {
    await publisher?.stop();
    await server?.stop();
  });

  test("measures steady-state ingest-to-glass after 30 seconds of playback", async ({ page, browserName }) => {
    await exposeHlsInstance(page);

    await page.addInitScript(() => {
      (window as any).__fatalHlsError = null;
    });

    const baseUrl = browserName === "webkit" ? server.httpsBaseUrl || server.baseUrl : server.baseUrl;
    await page.goto(`${baseUrl}/play/${STREAM_NAME}`);

    // Attach hls.js error listener.
    await page
      .waitForFunction(() => !!(window as any).__hlsInstance, null, { timeout: 5_000 })
      .catch(() => {
        /* WebKit / native HLS: no hls.js instance to attach to, by design. */
      });
    await page.evaluate(() => {
      const hls = (window as any).__hlsInstance;
      if (hls && hls.on && (window as any).Hls) {
        hls.on((window as any).Hls.Events.ERROR, (_evt: unknown, data: any) => {
          if (data?.fatal) {
            (window as any).__fatalHlsError = `${data.type}: ${data.details}`;
          }
        });
      }
    });

    // Wait for video to be ready and playing.
    const video = page.locator("#v");
    await expect
      .poll(async () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).readyState), {
        timeout: 20_000,
      })
      .toBeGreaterThanOrEqual(3);

    // Force play if needed.
    const isMuted = await page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).muted);
    if (!isMuted) {
      throw new Error("play.html's <video> is not muted; expected muted autoplay per the brief");
    }
    const isPaused = await page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).paused);
    if (isPaused) {
      console.log(`[${browserName}] video did not autoplay on its own; clicking play via Playwright`);
      await video.click();
    }

    // Wait for video width to confirm playback started.
    await expect
      .poll(async () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).videoWidth), {
        timeout: 20_000,
      })
      .toBe(1280);

    // Wait for at least 1 second of playback to stabilize before sampling.
    await expect
      .poll(async () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).currentTime), { timeout: 20_000 })
      .toBeGreaterThan(1);

    // Sample startup latency (before the 10-second wait).
    const startupMetrics = await readMetrics(page, browserName);
    const startupIngestToGlassSec = startupMetrics.ingestToGlassSec;

    console.log(`[${browserName}] startup ingest-to-glass: ${startupIngestToGlassSec !== null ? startupIngestToGlassSec.toFixed(2) + "s" : "n/a"}`);

    // Wait 10 seconds before starting steady-state sampling.
    await page.waitForTimeout(10_000);

    // Sample ingest-to-glass once per second for 20 seconds (to get 20 samples).
    const samples: number[] = [];
    const sampleIntervalMs = 1_000;
    const totalSampleTimeMs = 20_000;
    const startTime = Date.now();

    while (Date.now() - startTime < totalSampleTimeMs) {
      const partial = await readMetrics(page, browserName);
      if (partial.ingestToGlassSec !== null && isFinite(partial.ingestToGlassSec)) {
        samples.push(partial.ingestToGlassSec);
      }
      // Sleep until next sampling interval.
      const elapsed = Date.now() - startTime;
      const nextSampleAt = Math.ceil(elapsed / sampleIntervalMs) * sampleIntervalMs;
      const waitTime = nextSampleAt - elapsed;
      if (waitTime > 0) {
        await page.waitForTimeout(Math.min(waitTime, sampleIntervalMs));
      }
    }

    // Calculate statistics from samples.
    let steadyStateMin: number | null = null;
    let steadyStateMedian: number | null = null;
    let steadyStateMax: number | null = null;

    if (samples.length > 0) {
      samples.sort((a, b) => a - b);
      steadyStateMin = samples[0];
      steadyStateMax = samples[samples.length - 1];
      const midIdx = Math.floor(samples.length / 2);
      steadyStateMedian = samples.length % 2 === 1 ? samples[midIdx] : (samples[midIdx - 1] + samples[midIdx]) / 2;
    }

    const metricsSnapshot = await readMetrics(page, browserName);

    const result: SteadyStateMetrics = {
      browser: browserName,
      startupIngestToGlassSec,
      steadyStateMin,
      steadyStateMedian,
      steadyStateMax,
      validSampleCount: samples.length,
      fatalHlsError: metricsSnapshot.fatalHlsError,
      hlsInstanceExposed: metricsSnapshot.hlsInstanceExposed,
      engine: metricsSnapshot.engine,
    };

    console.log(`[${browserName}] steady-state samples collected: ${samples.length}`);
    console.log(`[${browserName}] steady-state ingest-to-glass: min=${steadyStateMin?.toFixed(2) ?? "n/a"}s, median=${steadyStateMedian?.toFixed(2) ?? "n/a"}s, max=${steadyStateMax?.toFixed(2) ?? "n/a"}s`);
    console.log(`[${browserName}] hls.js instance exposed: ${metricsSnapshot.hlsInstanceExposed} (engine: ${metricsSnapshot.engine})`);

    // Write results to latest.json under "steady" key.
    const resultsDir = await ensureResultsDir();
    const outFile = join(resultsDir, "latest.json");
    let existing: Record<string, unknown> = {};
    try {
      const raw = await import("node:fs/promises").then((fs) => fs.readFile(outFile, "utf8"));
      existing = JSON.parse(raw);
    } catch {
      // First run; nothing to merge yet.
    }

    if (!existing.steady) {
      existing.steady = {};
    }
    (existing.steady as Record<string, SteadyStateMetrics>)[browserName] = result;

    await writeFile(outFile, JSON.stringify(existing, null, 2) + "\n", "utf8");

    // Assertions.
    expect(samples.length, "at least 15 valid steady-state samples required").toBeGreaterThanOrEqual(15);
    expect(metricsSnapshot.fatalHlsError).toBeNull();

    expect(steadyStateMedian, "steady-state median ingest-to-glass should be measurable").not.toBeNull();
    const median = steadyStateMedian as number;
    if (!metricsSnapshot.hlsInstanceExposed) {
      // Apple's native player (macOS WebKit/Safari) needs HTTP/2 for low-latency
      // mode: ~5.5 s over HTTP/1.1. Over HTTPS/HTTP/2 it is BIMODAL (18 Sep 2026,
      // 6 runs): it joins either in low-latency mode (0.4-0.85 s) or in normal
      // mode (4.1-4.4 s) and stays there. Suspected cause: the missing
      // EXT-X-RENDITION-REPORT (Apple -50125, single rendition). Until that is
      // resolved, guard against regressions and record which mode it chose.
      if (median >= 3) {
        test.info().annotations.push({ type: "known-gap", description: `native HLS joined in normal mode: ${median.toFixed(2)} s (bimodal, see STATUS.md)` });
      }
      expect(median, "native HLS latency regressed beyond both known modes").toBeLessThan(5);
    } else {
      expect(median, "hls.js steady-state median ingest-to-glass should be < 3 s").toBeLessThan(3);
    }
  });
});
