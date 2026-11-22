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
      (window as any).__events = [];
      const v = document.getElementById("v") as HTMLVideoElement;
      for (const ev of ["ended", "waiting", "emptied", "durationchange", "pause", "play", "playing"]) {
        v.addEventListener(ev, () =>
          (window as any).__events.push(`${ev}@${v.currentTime.toFixed(2)}/${v.duration}`),
        );
      }
      if (hls && (window as any).Hls) {
        hls.on((window as any).Hls.Events.BUFFER_EOS, () => (window as any).__events.push("hls:BUFFER_EOS"));
        hls.on((window as any).Hls.Events.ERROR, (_e: unknown, d: any) => {
          if (d?.fatal) (window as any).__fatal.push(`${d.type}: ${d.details}`);
        });
      }
    });

    // The encoder drops...
    await publisher.stop();
    const atDrop = await time();
    await page.waitForTimeout(GAP_MS);
    const gapEvents = await page.evaluate(() => (window as any).__events as string[]);
    console.log(`[${browserName}] events during the gap: ${JSON.stringify(gapEvents)}`);
    expect(await ended(), `the player ended during the gap; events: ${JSON.stringify(gapEvents)}`).toBe(false);

    // ...and comes back under the same name. From here on, record the seam
    // window so a stall says why: currentTime/readyState/buffered every
    // 250 ms, the element's waiting/playing/pause/play/seeking events, and
    // hls.js appends, fragment changes and non-fatal errors.
    await page.evaluate(() => {
      const w = window as any;
      const v = document.getElementById("v") as HTMLVideoElement;
      const t0 = performance.now();
      const stamp = () => `${((performance.now() - t0) / 1000).toFixed(2)}s`;
      const ranges = () => {
        const out: string[] = [];
        for (let i = 0; i < v.buffered.length; i++) out.push(`${v.buffered.start(i).toFixed(2)}-${v.buffered.end(i).toFixed(2)}`);
        return out.join(",");
      };
      w.__seam = [] as string[];
      const log = (s: string) => {
        if (w.__seam.length < 400) w.__seam.push(`${stamp()} ${s}`);
      };
      for (const ev of ["waiting", "playing", "pause", "play", "seeking", "seeked", "stalled"]) {
        v.addEventListener(ev, () => log(`${ev}@${v.currentTime.toFixed(2)} [${ranges()}]`));
      }
      w.__seamTimer = setInterval(() => {
        const hls = w.__hlsInstance;
        const lat = hls && Number.isFinite(hls.latency) ? hls.latency.toFixed(2) : "-";
        const tgt = hls && hls.targetLatency != null ? hls.targetLatency.toFixed(2) : "-";
        log(
          `t=${v.currentTime.toFixed(2)} rs=${v.readyState} ${v.paused ? "paused" : "playing"} rate=${v.playbackRate} ` +
            `lat=${lat}/${tgt} [${ranges()}]`,
        );
      }, 250);
      const hls = w.__hlsInstance;
      const E = w.Hls?.Events;
      if (hls && E) {
        hls.on(E.FRAG_CHANGED, (_e: unknown, d: any) =>
          log(`hls:FRAG_CHANGED sn=${d.frag?.sn} cc=${d.frag?.cc} start=${d.frag?.start?.toFixed(2)}`),
        );
        hls.on(E.BUFFER_APPENDED, (_e: unknown, d: any) => {
          const f = d.frag ?? {};
          const p = d.part ? `.p${d.part.index}` : "";
          log(`hls:APPENDED ${d.type} sn=${f.sn}${p} cc=${f.cc} [${ranges()}]`);
        });
        hls.on(E.ERROR, (_e: unknown, d: any) => {
          if (!d?.fatal) log(`hls:ERROR ${d?.details}${d?.buffer != null ? ` buffer=${Number(d.buffer).toFixed(2)}` : ""}`);
        });
      }
    });
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
    const latencyAtT0 = await page.evaluate(() => {
      const hls = (window as any).__hlsInstance;
      return hls && Number.isFinite(hls.latency) ? `${hls.latency.toFixed(2)}/${hls.targetLatency?.toFixed(2)}` : "-";
    });
    await page.waitForTimeout(3_000);
    const delta = (await time()) - t0;

    const info = await page.evaluate(() => ({
      seam: (() => {
        clearInterval((window as any).__seamTimer);
        return ((window as any).__seam ?? []) as string[];
      })(),
      fatal: (window as any).__fatal as string[] | undefined,
      rebuilds: (window as any).__hlsCount ?? 0,
      native: !(window as any).__hlsInstance,
    }));
    console.log(
      `[${browserName}] engine=${info.native ? "native" : "hls.js"} currentTime at drop=${atDrop.toFixed(2)} ` +
        `passed atDrop+8 s ${resumedAfterMs} ms after the republish; ` +
        `then ${delta.toFixed(2)} s in 3 s; hls.js instances=${info.rebuilds}; fatal errors=${JSON.stringify(info.fatal ?? [])}`,
    );
    const holds = info.seam.filter((l) => / pause@/.test(l)).length;
    const waits = info.seam.filter((l) => / waiting@/.test(l)).length;
    const errors = [...new Set(info.seam.filter((l) => l.includes("hls:ERROR")).map((l) => l.split("hls:ERROR ")[1]))];
    console.log(`[${browserName}] seam window: latency/target ${latencyAtT0} before the 3 s sample, ${waits} waiting, ${holds} pause(s), non-fatal hls.js errors ${JSON.stringify(errors)}`);
    if (delta < 2.4) console.log(`[${browserName}] seam log (since the republish):\n  ${info.seam.join("\n  ")}`);
    expect(await ended()).toBe(false);
    expect(delta, "playback stalled after the reconnect").toBeGreaterThanOrEqual(2.4);
    // The playlist continued, so hls.js never had to be torn down and rebuilt.
    expect(info.fatal ?? []).toEqual([]);
  });
});
