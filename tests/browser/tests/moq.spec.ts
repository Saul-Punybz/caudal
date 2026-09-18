// Media over QUIC playback in a real browser, through the Caudal web UI:
// RTMP in (H.264 + AAC) -> caudal-moq republishes it as a `hang` broadcast
// -> /streams/<name> -> "MoQ" -> `@moq/watch`'s <moq-watch> decodes it with
// WebCodecs and paints to a <canvas> (there is no <video>/MediaStream in
// this path, unlike LL-HLS and WHEP).
//
// The server side (crates/caudal-moq, agent P) is developed in parallel in
// a different worktree (STATUS.md "Batch 5"). In *this* worktree its
// `start()` is still the scaffold stub that always returns
// `Err("MoQ not implemented yet")` (see crates/caudal-moq/src/lib.rs), so
// `GET /moq/fingerprint` is not wired into the router yet and this test
// skips itself with a clear message instead of failing deep inside a
// Playwright assertion. It will go red (in the good way: actually running)
// once the orchestrator merges P's crate.

import { test, expect } from "@playwright/test";
import { haveFfmpeg, startCaudal, startFfmpegPublisher, type CaudalServer, type FfmpegPublisher } from "./harness";

const STREAM = "moq";

interface MoqFingerprint {
  url: string;
  fingerprint: string | null;
}

test.describe("MoQ playback in a real browser", () => {
  test.skip(!haveFfmpeg(), "ffmpeg is not installed");
  // Runs on Chromium and Firefox, which both have WebTransport (with
  // serverCertificateHashes) and WebCodecs; verified 18 Sep 2026. WebKit /
  // Safari has no WebTransport yet, matching the UI's `window.WebTransport` gate.
  test.skip(({ browserName }) => browserName === "webkit", "MoQ needs WebTransport, which WebKit lacks");

  let server: CaudalServer;
  let publisher: FfmpegPublisher;
  let moq: MoqFingerprint | null = null;

  test.beforeAll(async () => {
    server = await startCaudal();
    publisher = startFfmpegPublisher(server.rtmpUrl(STREAM));
    const deadline = Date.now() + 15_000;
    while (Date.now() < deadline) {
      const r = await fetch(`${server.baseUrl}/api/v1/streams/${STREAM}`).catch(() => null);
      if (r && r.status === 200 && (await r.text()).includes('"h264"')) break;
      await new Promise((res) => setTimeout(res, 250));
    }

    // Probe the server contract up front (STATUS.md "Batch 5":
    // `GET /moq/fingerprint` -> `{"url": "...", "fingerprint": "..." | null}`).
    // Anything else — 404 (route not merged), 503, the SPA's index.html
    // (the UI router's catch-all, which is what an unmerged route actually
    // falls through to today), or a network error — means P hasn't landed.
    try {
      const r = await fetch(`${server.baseUrl}/moq/fingerprint`, { headers: { accept: "application/json" } });
      if (r.ok) {
        const body = (await r.json()) as Partial<MoqFingerprint>;
        if (body && typeof body.url === "string" && body.url.length > 0) {
          moq = { url: body.url, fingerprint: typeof body.fingerprint === "string" ? body.fingerprint : null };
        }
      }
    } catch {
      // Not JSON (e.g. the SPA fallback's HTML), connection refused, etc.
    }
  });

  test.afterAll(async () => {
    await publisher?.stop();
    await server?.stop();
  });

  test("plays over Media over QUIC from the stream page", async ({ page, browserName }) => {
    test.skip(
      moq === null,
      "GET /moq/fingerprint isn't available in this worktree yet — crates/caudal-moq (agent P) " +
        "is still the scaffold stub (`start()` returns Err). Expected to fail until the orchestrator " +
        "merges P's branch; see STATUS.md \"Batch 5\".",
    );

    await page.goto(`${server.baseUrl}/streams/${STREAM}`);

    const moqRadio = page.getByRole("radio", { name: "MoQ" });
    await expect(moqRadio, "MoQ option should be enabled: this Chromium build has WebTransport").toBeEnabled();
    await moqRadio.click();

    // @moq/watch has no <video>/MediaStream in this path: WebCodecs decodes
    // straight to VideoFrames, drawn onto a <canvas> nested in <moq-watch>.
    const canvas = page.locator("moq-watch canvas");
    await expect
      .poll(async () => canvas.evaluate((c: HTMLCanvasElement) => c.width).catch(() => 0), { timeout: 20_000 })
      .toBe(1280);
    await expect
      .poll(async () => canvas.evaluate((c: HTMLCanvasElement) => c.height).catch(() => 0), { timeout: 5_000 })
      .toBe(720);

    // frameCount comes straight from `<moq-watch>.video.out.stats` (see
    // node_modules/@moq/watch/video/decoder.d.ts) — the same public signal
    // the library's own stats panel reads, not something invented for this
    // test.
    const frameCount = () =>
      page.evaluate(() => {
        const el = document.querySelector("moq-watch") as unknown as {
          video: { out: { stats: { peek(): { frameCount: number } | undefined } } };
        } | null;
        return el?.video.out.stats.peek()?.frameCount ?? 0;
      });

    const f0 = await frameCount();
    await page.waitForTimeout(3_000);
    const f1 = await frameCount();
    console.log(`[${browserName}] MoQ decoded ${f1 - f0} video frames in 3 s`);
    // 30fps source; a generous floor (10fps) so this isn't flaky on a slow
    // CI runner while still catching a real stall (0 or near-0 frames).
    expect(f1 - f0, "MoQ video playback stalled").toBeGreaterThanOrEqual(30);

    // Audio is a best-effort report, not an assertion: WebCodecs' AudioDecoder
    // support for AAC varies, and whether caudal-moq's catalog even offers
    // AAC in a WebCodecs-compatible form is still P's implementation detail.
    const audioBytes = await page.evaluate(() => {
      const el = document.querySelector("moq-watch") as unknown as {
        audio: { out: { stats: { peek(): { bytesReceived: number } | undefined } } };
      } | null;
      return el?.audio.out.stats.peek()?.bytesReceived ?? 0;
    });
    console.log(
      audioBytes > 0
        ? `[${browserName}] MoQ audio decoding: ${audioBytes} bytes received`
        : `[${browserName}] MoQ audio absent or not decoding (source is AAC; see moq.spec.ts comment)`,
    );

    const jitter = await page.getByText(/ms jitter buffer/).first().textContent().catch(() => null);
    console.log(`[${browserName}] UI jitter-buffer readout: ${jitter ?? "n/a"}`);
  });
});
