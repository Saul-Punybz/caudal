// Audit gap 4: the encoder drops and republishes the same stream name
// within [hls] reconnect_grace_secs (default 10 s). The LL-HLS playlist
// continues with an EXT-X-DISCONTINUITY instead of ending, so the player on
// /play/{name} must keep going on its own: the <video> never reaches
// `ended`, and currentTime moves past where it was when the publisher left.
//
// One caudal binary + one stream name per browser project, like play.spec.ts.

import { test, expect } from "@playwright/test";
import {
  haveFfmpeg,
  startCaudal,
  startFfmpegPublisher,
  type CaudalServer,
  type FfmpegPublisher,
} from "./harness";
import { exposeHlsInstance } from "./metrics";

const STREAM_NAME = "reconnect";
/** How long the encoder stays away: a quick restart, well inside the grace. */
const GAP_MS = 3_000;

test.describe("LL-HLS survives a publisher reconnect", () => {
  test.skip(!haveFfmpeg(), "ffmpeg is not installed");

  let server: CaudalServer;
  let publisher: FfmpegPublisher | undefined;

  test.beforeAll(async ({ browserName }) => {
    server = await startCaudal({ tls: browserName === "webkit" });
  });

  test.afterAll(async () => {
    await publisher?.stop();
    await server?.stop();
  });

  test("video keeps advancing across a drop and republish of the same name", async ({ page, browserName }) => {
    test.setTimeout(120_000);
    publisher = startFfmpegPublisher(server.rtmpUrl(STREAM_NAME));
    await exposeHlsInstance(page);

    const baseUrl = browserName === "webkit" ? server.httpsBaseUrl || server.baseUrl : server.baseUrl;
    await page.goto(`${baseUrl}/play/${STREAM_NAME}`);
    // The page retries until the stream exists; give it the first segments.
    const time = () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).currentTime);
    const ended = () => page.evaluate(() => (document.getElementById("v") as HTMLVideoElement).ended);
    await expect.poll(time, { timeout: 30_000 }).toBeGreaterThan(3);

    await page.evaluate(() => {
      const hls = (window as any).__hlsInstance;
      (window as any).__fatal = [];
      if (hls && (window as any).Hls) {
        hls.on((window as any).Hls.Events.ERROR, (_e: unknown, d: any) => {
          if (d?.fatal) (window as any).__fatal.push(`${d.type}: ${d.details}`);
        });
      }
    });

    // The encoder drops...
    await publisher.stop();
    const atDrop = await time();
    await page.waitForTimeout(GAP_MS);
    expect(await ended(), "the player ended during the gap").toBe(false);

    // ...and comes back under the same name.
    const back = Date.now();
    publisher = startFfmpegPublisher(server.rtmpUrl(STREAM_NAME));
    // At the drop the player sits a few seconds (its latency) behind the
    // last frame of the old publish; 8 s past `atDrop` is past the seam.
    await expect
      .poll(time, { timeout: 25_000, message: "currentTime never moved past the reconnect" })
      .toBeGreaterThan(atDrop + 8);
    const resumedAfterMs = Date.now() - back;

    // And it keeps playing smoothly on the new publish, once it has settled
    // (native HLS may catch up in small steps right after the seam).
    await page.waitForTimeout(2_000);
    const t0 = await time();
    await page.waitForTimeout(3_000);
    const delta = (await time()) - t0;

    const info = await page.evaluate(() => ({
      fatal: (window as any).__fatal as string[] | undefined,
      rebuilds: (window as any).__hlsCount ?? 0,
      native: !(window as any).__hlsInstance,
    }));
    console.log(
      `[${browserName}] engine=${info.native ? "native" : "hls.js"} currentTime at drop=${atDrop.toFixed(2)} ` +
        `passed atDrop+8 s ${resumedAfterMs} ms after the republish; ` +
        `then ${delta.toFixed(2)} s in 3 s; hls.js instances=${info.rebuilds}; fatal errors=${JSON.stringify(info.fatal ?? [])}`,
    );
    expect(await ended()).toBe(false);
    expect(delta, "playback stalled after the reconnect").toBeGreaterThanOrEqual(2.4);
    // The playlist continued, so hls.js never had to be torn down and rebuilt.
    expect(info.fatal ?? []).toEqual([]);
  });
});
