// WebRTC playback (WHEP) in a real browser, through the Caudal web UI:
// RTMP in (H.264 + AAC) -> /streams/<name> -> "WebRTC" -> the <video> plays.
// The source is AAC, so WebRTC is video-only by design and the UI says so.

import { test, expect } from "@playwright/test";
import { haveFfmpeg, startCaudal, startFfmpegPublisher, type CaudalServer, type FfmpegPublisher } from "./harness";

const STREAM = "whep";

test.describe("WHEP playback in a real browser", () => {
  test.skip(!haveFfmpeg(), "ffmpeg is not installed");
  // Apple's WebKit build in Playwright has no H.264 WebRTC decoder on Linux,
  // and native Safari is covered by HLS; WHEP runs on Chromium and Firefox.
  test.skip(({ browserName }) => browserName === "webkit", "WHEP is tested on Chromium and Firefox");

  let server: CaudalServer;
  let publisher: FfmpegPublisher;

  test.beforeAll(async () => {
    server = await startCaudal();
    publisher = startFfmpegPublisher(server.rtmpUrl(STREAM));
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      const r = await fetch(`${server.baseUrl}/api/v1/streams/${STREAM}`).catch(() => null);
      if (r && r.status === 200 && (await r.text()).includes('"h264"')) return;
      await new Promise((res) => setTimeout(res, 250));
    }
    throw new Error("stream never appeared");
  });

  test.afterAll(async () => {
    await publisher?.stop();
    await server?.stop();
  });

  test("plays over WebRTC from the stream page", async ({ page, browserName }) => {
    await page.goto(`${server.baseUrl}/streams/${STREAM}`);
    await page.getByRole("radio", { name: "WebRTC" }).click();

    const video = page.locator("video");
    await expect.poll(async () => video.evaluate((v: HTMLVideoElement) => v.readyState), { timeout: 20_000 }).toBeGreaterThanOrEqual(3);
    await expect.poll(async () => video.evaluate((v: HTMLVideoElement) => v.videoWidth), { timeout: 10_000 }).toBe(1280);

    const t0 = await video.evaluate((v: HTMLVideoElement) => v.currentTime);
    await page.waitForTimeout(3_000);
    const t1 = await video.evaluate((v: HTMLVideoElement) => v.currentTime);
    console.log(`[${browserName}] WHEP currentTime advanced ${(t1 - t0).toFixed(2)} s in 3 s`);
    expect(t1 - t0, "WebRTC playback stalled").toBeGreaterThanOrEqual(2.4);

    const buffer = await page.getByText(/ms buffer/).first().textContent().catch(() => null);
    console.log(`[${browserName}] UI buffer readout: ${buffer ?? "n/a"}`);
    await expect(page.getByText(/audio not available over WebRTC/)).toBeVisible();
  });
});
