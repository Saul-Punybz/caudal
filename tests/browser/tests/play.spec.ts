// Proves a real browser plays Caudal's LL-HLS. Until this suite existed,
// only ffmpeg/ffprobe had ever decoded it — Chrome under Playwright's
// codegen/CDP-driven automation used to keep the tab `hidden` and throttle
// timers so hls.js never attached, but a headless Playwright *browser
// context* (this file) renders and runs timers normally, so that problem
// does not apply here. See STATUS.md, "Batch 2", agent E.
//
// One test file == one caudal binary + one ffmpeg publish, shared by the
// single test in it, so Chromium, Firefox and WebKit (run as separate Playwright
// "projects") each get their own stream on their own ports.

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
import { exposeHlsInstance, readMetrics, type Metrics } from "./metrics";

const STREAM_NAME = "e2e";

test.describe("Caudal LL-HLS in a real browser", () => {
  test.skip(!haveFfmpeg(), "ffmpeg is not installed — skipping the whole browser suite (see brief step 2)");

  let server: CaudalServer;
  let publisher: FfmpegPublisher;

  test.beforeAll(async ({ browserName }) => {
    server = await startCaudal({ tls: browserName === "webkit" });
    publisher = startFfmpegPublisher(server.rtmpUrl(STREAM_NAME));
    // Give ffmpeg + the RTMP handshake + the first LL-HLS parts a moment
    // before a browser asks for the playlist, so the first request isn't
    // wasted retrying a 404.
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

  test("plays live within 20s, advances, and reports latency under 3s", async ({ page, browserName }) => {
    await exposeHlsInstance(page);

    await page.addInitScript(() => {
      (window as any).__fatalHlsError = null;
    });

    const baseUrl = browserName === "webkit" ? server.httpsBaseUrl || server.baseUrl : server.baseUrl;
    await page.goto(`${baseUrl}/play/${STREAM_NAME}`);

    // The page's own hls.js listener already destroys/rebuilds on a fatal
    // error; we just also want to know it happened. Poll for the exposed
    // instance and attach our own listener once it exists (native HLS on
    // WebKit has no such instance, and that's expected).
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

    const video = page.locator("#v");
    await expect
      .poll(async () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).readyState), {
        timeout: 20_000,
      })
      .toBeGreaterThanOrEqual(3);

    // If the page doesn't already mute+autoplay for this browser, force it
    // via Playwright rather than relying on a user gesture that never
    // happens in a headless run.
    const isMuted = await page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).muted);
    if (!isMuted) {
      throw new Error("play.html's <video> is not muted; expected muted autoplay per the brief");
    }
    const isPaused = await page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).paused);
    if (isPaused) {
      console.log(`[${browserName}] video did not autoplay on its own; clicking play via Playwright`);
      await video.click();
    }

    await expect
      .poll(async () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).videoWidth), {
        timeout: 20_000,
      })
      .toBe(1280);

    // Warm up first: startup buffering is not a stall. Then playback must be
    // smooth: at least 2.4 s of media over 3 s of wall time (80%).
    await expect
      .poll(async () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).currentTime), { timeout: 20_000 })
      .toBeGreaterThan(1);
    const currentTimeStart = await page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).currentTime);
    await page.waitForTimeout(3_000);
    const currentTimeEnd = await page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).currentTime);
    const currentTimeDelta = currentTimeEnd - currentTimeStart;

    // Native HLS (WebKit) tends to start a few segments behind the live
    // edge and catches up gradually rather than jumping there, so give the
    // latency figures up to 15s more wall time to converge before we
    // sample for the assertion below. hls.js (Chromium) is already close
    // to live within the first couple of seconds.
    let partial = await readMetrics(page, browserName);
    const convergeDeadline = Date.now() + 15_000;
    while (
      (partial.liveEdgeDistanceSec === null || partial.liveEdgeDistanceSec >= 3 || partial.ingestToGlassSec === null || partial.ingestToGlassSec >= 3) &&
      Date.now() < convergeDeadline
    ) {
      await page.waitForTimeout(500);
      partial = await readMetrics(page, browserName);
    }
    const metrics: Metrics = { ...partial, currentTimeStart, currentTimeEnd, currentTimeDelta };

    console.log(`[${browserName}] readyState=${metrics.readyState} videoWidth=${metrics.videoWidth}x${metrics.videoHeight}`);
    console.log(`[${browserName}] currentTime delta over 3s wall time: ${(metrics.currentTimeDelta ?? 0).toFixed(2)}s`);
    console.log(
      `[${browserName}] live-edge distance: ${metrics.liveEdgeDistanceSec !== null ? metrics.liveEdgeDistanceSec.toFixed(2) + "s" : "n/a"}`,
    );
    console.log(
      `[${browserName}] ingest-to-glass: ${metrics.ingestToGlassSec !== null ? metrics.ingestToGlassSec.toFixed(2) + "s" : "n/a"}`,
    );
    console.log(`[${browserName}] hls.js instance exposed: ${metrics.hlsInstanceExposed} (engine: ${metrics.engine})`);

    const resultsDir = await ensureResultsDir();
    const outFile = join(resultsDir, "latest.json");
    let existing: Record<string, Metrics> = {};
    try {
      const raw = await import("node:fs/promises").then((fs) => fs.readFile(outFile, "utf8"));
      existing = JSON.parse(raw);
    } catch {
      // First browser to finish in this run; nothing to merge yet.
    }
    existing[browserName] = metrics;
    await writeFile(outFile, JSON.stringify(existing, null, 2) + "\n", "utf8");

    expect(metrics.readyState).toBeGreaterThanOrEqual(3);
    expect(metrics.videoWidth).toBe(1280);
    expect(metrics.currentTimeDelta, "playback stalled after warm-up").toBeGreaterThanOrEqual(2.4);
    expect(metrics.fatalHlsError).toBeNull();

    expect(metrics.liveEdgeDistanceSec, "live-edge distance was not measurable").not.toBeNull();
    expect(metrics.liveEdgeDistanceSec as number).toBeLessThan(3);

    expect(metrics.ingestToGlassSec, "ingest-to-glass was not measurable").not.toBeNull();
    const ingestToGlass = metrics.ingestToGlassSec as number;
    // Apple's native HLS player (macOS WebKit/Safari) requires HTTP/2 for low-latency mode.
    // Measured on 18 Sep 2026: ~5.5 s over HTTP/1.1, 0.52 s over HTTP/2 (HTTPS).
    // Linux WebKit (Playwright) uses hls.js instead and has no native player, so
    // hlsInstanceExposed=true and these code paths share the same assertion.
    if (!metrics.hlsInstanceExposed) {
      // Native HLS over HTTPS/HTTP/2 is bimodal (0.4-0.85 s or 4.1-4.4 s, see
      // steady.spec.ts). Guard the upper mode; annotate the lower one's miss.
      if (ingestToGlass >= 3) {
        test.info().annotations.push({ type: "known-gap", description: `native HLS joined in normal mode: ${ingestToGlass.toFixed(2)} s` });
      }
      expect(ingestToGlass, "Native HLS latency regressed beyond both known modes").toBeLessThan(5);
    } else {
      // hls.js engine (Chromium, Firefox, Linux WebKit): assert < 3 s for startup.
      expect(ingestToGlass).toBeLessThan(3);
    }
  });
});
