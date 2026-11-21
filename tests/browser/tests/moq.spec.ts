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
import { haveFfmpeg, startCaudal, startFfmpegPublisher, logMachineLoad, type CaudalServer, type FfmpegPublisher } from "./harness";

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
    // Debug for the MoQ/QUIC stack: printed only when the test fails (the
    // firefox flake is a session that never comes up; NOTES.md).
    server = await startCaudal({
      rustLog: "info,caudal_moq=debug,moq_native=debug,moq_net=debug,web_transport_quinn=debug,quinn=debug,quinn_proto=debug",
    });
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

  test.afterEach(async ({ browserName }, testInfo) => {
    if (testInfo.status === testInfo.expectedStatus || !server) return;
    const lines = server.logs().split("\n");
    const moqLines = lines.filter((l) => /moq|quic|quinn|webtransport|h3|tls/i.test(l));
    console.log(
      `[${browserName}] server log on failure: ${moqLines.length} MoQ/QUIC lines of ${lines.length}\n` +
        moqLines.slice(-300).join("\n"),
    );
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

    // Diagnostics for the firefox-only canvas-resize flake (seen twice: the
    // canvas never reached width 1280; passed on rerun; an untouched-page
    // control rerun also passed — so not a global runner problem by itself).
    // Forward the page's own console (moq/watch, WebTransport, @moq/hang
    // libraries tend to log connection/subscribe state) and any uncaught
    // page error, prefixed so they interleave with the harness's log lines.
    page.on("console", (msg) => console.log(`[${browserName}][page:${msg.type()}] ${msg.text()}`));
    page.on("pageerror", (err) => console.log(`[${browserName}][pageerror] ${err.stack || err.message}`));
    logMachineLoad();

    // The firefox flake: the server never sees a session, and @moq/net only
    // logs in dev builds. Wrap the two transports it races so the page log
    // says what the browser did: which URL, and whether WebTransport's
    // `ready` resolved, rejected, or never settled (a hung handshake never
    // reaches @moq/net's retry loop). Logged as `[wt]` / `[ws]` lines.
    await page.addInitScript(() => {
      const t0 = performance.now();
      const at = () => `+${Math.round(performance.now() - t0)}ms`;
      const WT = (window as any).WebTransport;
      if (WT) {
        (window as any).WebTransport = new Proxy(WT, {
          construct(target, args) {
            const opts = args[1] ?? {};
            console.log(`[wt] new ${args[0]} ${at()} hashes=${(opts.serverCertificateHashes ?? []).length} protocols=${(opts.protocols ?? []).length}`);
            const wt = new target(...args);
            wt.ready.then(
              () => console.log(`[wt] ready ${at()}`),
              (e: unknown) => console.log(`[wt] ready rejected ${at()}: ${e}`),
            );
            wt.closed.then(
              (i: unknown) => console.log(`[wt] closed ${at()}: ${JSON.stringify(i)}`),
              (e: unknown) => console.log(`[wt] closed with error ${at()}: ${e}`),
            );
            return wt;
          },
        });
      }
      const WS = window.WebSocket;
      (window as any).WebSocket = new Proxy(WS, {
        construct(target, args) {
          console.log(`[ws] new ${args[0]} ${at()}`);
          const ws = new target(...(args as [string]));
          ws.addEventListener("open", () => console.log(`[ws] open ${at()}`));
          ws.addEventListener("error", () => console.log(`[ws] error ${at()}`));
          return ws;
        },
      });
    });
    await page.goto(`${server.baseUrl}/streams/${STREAM}`);

    const moqRadio = page.getByRole("radio", { name: "MoQ" });
    await expect(moqRadio, "MoQ option should be enabled: this Chromium build has WebTransport").toBeEnabled();
    // The 21 Nov failure (NOTES.md, "Firefox MoQ flake: the click"): the web
    // fonts swapped in during the click, the segmented button moved, and
    // the click never reached React — aria-checked stayed false and LL-HLS
    // kept playing, so no WebTransport was ever constructed. The UI now
    // reserves icon widths; label text can still reflow when Roboto Flex
    // arrives, so wait for the fonts, then confirm the switch took and say
    // so loudly if it needed another click. Clicking twice is what a viewer
    // would do; it does not hide anything on the MoQ path, which only
    // starts once the switch has happened.
    await page.evaluate(() => document.fonts.ready.then(() => undefined));
    const radioClickedAt = Date.now();
    let clicks = 0;
    await expect(async () => {
      if ((await moqRadio.getAttribute("aria-checked")) !== "true") {
        clicks++;
        await moqRadio.click();
      }
      await expect(moqRadio).toHaveAttribute("aria-checked", "true", { timeout: 2_000 });
    }).toPass({ timeout: 10_000 });
    if (clicks > 1) {
      console.log(`[${browserName}] MoQ radio needed ${clicks} clicks before aria-checked=true (layout shift under the pointer)`);
    }

    // frameCount comes straight from `<moq-watch>.video.out.stats` (see
    // node_modules/@moq/watch/video/decoder.d.ts) — the same public signal
    // the library's own stats panel reads, not something invented for this
    // test. Read alongside canvas size during the wait below so a stalled
    // subscribe (frameCount stuck at 0) can be told apart from a decoded
    // stream whose <canvas> just hasn't been resized yet (a late-resize bug).
    const frameCount = () =>
      page.evaluate(() => {
        const el = document.querySelector("moq-watch") as unknown as {
          video: { out: { stats: { peek(): { frameCount: number } | undefined } } };
        } | null;
        return el?.video.out.stats.peek()?.frameCount ?? 0;
      });

    // @moq/watch has no <video>/MediaStream in this path: WebCodecs decodes
    // straight to VideoFrames, drawn onto a <canvas> nested in <moq-watch>.
    const canvas = page.locator("moq-watch canvas");
    await expect
      .poll(
        async () => {
          // page.evaluate, not canvas.evaluate: a locator waits for its
          // element, so with no <moq-watch> the first poll blocked for the
          // whole 20 s and logged once. This logs every poll, plus what the
          // player box is showing (its overlay text or error).
          const [{ width, height, mounted, overlay }, frames] = await Promise.all([
            page.evaluate(() => {
              const c = document.querySelector("moq-watch canvas") as HTMLCanvasElement | null;
              const box = document.querySelector('[role="alert"], [aria-live="polite"].pointer-events-none, [role="status"]');
              return {
                width: c?.width ?? 0,
                height: c?.height ?? 0,
                mounted: document.querySelector("moq-watch") !== null,
                overlay: box?.textContent?.trim() ?? "",
              };
            }),
            frameCount().catch(() => -1),
          ]);
          console.log(
            `[${browserName}] moq canvas width=${width} height=${height} frameCount=${frames} moq-watch=${mounted} overlay=${JSON.stringify(overlay)} @ +${Date.now() - radioClickedAt}ms`,
          );
          return width;
        },
        { timeout: 20_000 },
      )
      .toBe(1280);
    await expect
      .poll(async () => canvas.evaluate((c: HTMLCanvasElement) => c.height).catch(() => 0), { timeout: 5_000 })
      .toBe(720);

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
