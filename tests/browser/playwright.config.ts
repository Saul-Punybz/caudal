import { defineConfig, devices } from "@playwright/test";

// One binary + one ffmpeg publish per test file, so tests never share a
// stream name or port. Everything is real: no mocked network, no fake
// timers — hls.js and native HLS behave the same way they would for a
// person, which is the whole point of this suite (see STATUS.md, "Batch 2").
export default defineConfig({
  testDir: "./tests",
  timeout: 90_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"], ["json", { outputFile: "results/report.json" }]],
  use: {
    trace: "retain-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: {
        ...devices["Desktop Chrome"],
        // Muted autoplay works without this flag in headless Chromium, but
        // pass it explicitly per the brief so unmuted/no-gesture playback
        // is never blocked by the autoplay policy.
        launchOptions: {
          args: ["--autoplay-policy=no-user-gesture-required"],
        },
      },
    },
    {
      name: "webkit",
      use: {
        ...devices["Desktop Safari"],
        ignoreHTTPSErrors: true,
      },
    },
    {
      // Firefox plays through hls.js like Chromium, but on its own media
      // stack (MSE + its own decoders), so it catches different bugs.
      name: "firefox",
      // One retry in CI only. Firefox (last project, software-decoding 720p
      // through hls.js) stalled on LL-HLS in 3 of ~10 CI runs while the
      // 4-core runner's load was 3-4; no leftover processes (harness load
      // log, 19 Sep 2026), Chromium and WebKit never. A retried pass is
      // still reported as "flaky", so it stays visible.
      retries: process.env.CI ? 1 : 0,
      use: {
        ...devices["Desktop Firefox"],
        launchOptions: {
          // 0 = allow autoplay (muted or not) without a user gesture.
          firefoxUserPrefs: { "media.autoplay.default": 0 },
        },
      },
    },
  ],
});
