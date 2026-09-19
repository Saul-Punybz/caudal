// Backup source (PLAN batch 10): viewers play `fo`, fed by `fo-primary`.
// The primary's encoder dies; Caudal switches `fo` to `fo-backup` on its
// next keyframe with the timeline continuing, then back to the primary
// once it has been healthy for the hold. The player on /play/fo must ride
// both switches on its own: the <video> never reaches `ended`, hls.js
// raises no fatal error, and currentTime keeps advancing.
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

const FAILOVER = [
  "[[failover]]",
  'stream = "fo"',
  'sources = ["fo-primary", "fo-backup"]',
  "switch_after_ms = 1000",
  "switch_back_after_secs = 4",
].join("\n");

test.describe("LL-HLS rides a failover to a backup source and back", () => {
  test.skip(!haveFfmpeg(), "ffmpeg is not installed");

  let server: CaudalServer;
  let primary: FfmpegPublisher | undefined;
  let backup: FfmpegPublisher | undefined;

  test.beforeAll(async ({ browserName }) => {
    server = await startCaudal({ tls: browserName === "webkit", extraToml: FAILOVER });
  });

  test.afterAll(async () => {
    await primary?.stop();
    await backup?.stop();
    await server?.stop();
  });

  test("video keeps advancing across both switches", async ({ page, browserName }) => {
    test.setTimeout(120_000);
    primary = startFfmpegPublisher(server.rtmpUrl("fo-primary"));
    backup = startFfmpegPublisher(server.rtmpUrl("fo-backup"));
    await exposeHlsInstance(page);

    const active = async () => {
      const res = await fetch(`${server.baseUrl}/api/v1/failover`);
      return ((await res.json()) as { active: string | null }[])[0]?.active;
    };
    await expect.poll(active, { timeout: 20_000 }).toBe("fo-primary");

    const baseUrl = browserName === "webkit" ? server.httpsBaseUrl || server.baseUrl : server.baseUrl;
    await page.goto(`${baseUrl}/play/fo`);
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

    // The primary dies: the backup goes on air.
    await primary.stop();
    primary = undefined;
    const atDrop = await time();
    await expect.poll(active, { timeout: 10_000 }).toBe("fo-backup");
    await expect
      .poll(time, { timeout: 25_000, message: "currentTime never moved past the switch to the backup" })
      .toBeGreaterThan(atDrop + 6);
    expect(await ended(), "the player ended on the backup").toBe(false);

    // The primary returns: after the hold it is on air again.
    primary = startFfmpegPublisher(server.rtmpUrl("fo-primary"));
    await expect.poll(active, { timeout: 20_000 }).toBe("fo-primary");
    const atBack = await time();
    await expect
      .poll(time, { timeout: 25_000, message: "currentTime never moved past the switch back" })
      .toBeGreaterThan(atBack + 6);

    // And it plays smoothly on the primary once settled.
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
        `at switch back=${atBack.toFixed(2)}; then ${delta.toFixed(2)} s in 3 s; ` +
        `hls.js instances=${info.rebuilds}; fatal errors=${JSON.stringify(info.fatal ?? [])}`,
    );
    expect(await ended()).toBe(false);
    expect(delta, "playback stalled after switching back").toBeGreaterThanOrEqual(2.4);
    expect(info.fatal ?? []).toEqual([]);
  });
});
