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
    try {
      await expect.poll(async () => video.evaluate((v: HTMLVideoElement) => v.videoWidth), { timeout: 10_000 }).toBe(1280);
    } catch (e) {
      // No picture: say why, from a second, bare WHEP session's stats
      // (packets in but nothing decoded = the browser has no decoder).
      console.log(`[${browserName}] no video; bare WHEP session: ${await whepDiagnostics(page, STREAM)}`);
      throw e;
    }

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

/** Opens a bare WHEP session in the page for 5 s and returns its inbound
 * video stats (packets, frames decoded, decoder) and ICE state. */
async function whepDiagnostics(page: import("@playwright/test").Page, stream: string): Promise<string> {
  return page.evaluate(async (name) => {
    const pc = new RTCPeerConnection();
    pc.addTransceiver("video", { direction: "recvonly" });
    const offer = await pc.createOffer();
    await pc.setLocalDescription(offer);
    const r = await fetch(`/whep/${name}`, { method: "POST", headers: { "Content-Type": "application/sdp" }, body: offer.sdp });
    await pc.setRemoteDescription({ type: "answer", sdp: await r.text() });
    await new Promise((res) => setTimeout(res, 5000));
    const out: string[] = [`ice=${pc.iceConnectionState}`];
    const stats = await pc.getStats();
    stats.forEach((s: any) => {
      if (s.type === "inbound-rtp" && s.kind === "video") {
        const codec = s.codecId ? ((stats as any).get(s.codecId) as any) : undefined;
        out.push(`packets=${s.packetsReceived} framesDecoded=${s.framesDecoded} decoder=${s.decoderImplementation} codec=${codec?.mimeType} ${codec?.sdpFmtpLine ?? ""}`);
      }
    });
    pc.close();
    return out.join(" ");
  }, stream);
}
