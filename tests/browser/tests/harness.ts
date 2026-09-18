// Drives the real `caudal` binary and a real `ffmpeg` publisher, the same
// way `crates/caudal/tests/support/mod.rs` drives them for the Rust e2e
// suite. Nothing here is mocked: a real TCP/RTMP connection, a real TOML
// config file, a real child process.

import { spawn, spawnSync, type ChildProcess } from "node:child_process";
import { mkdtemp, rm, writeFile, mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import net from "node:net";
import dgram from "node:dgram";

export function haveFfmpeg(): boolean {
  const r = spawnSync("ffmpeg", ["-version"], { stdio: "ignore" });
  return r.status === 0;
}

/** A free TCP port, found the same way the Rust harness does: bind :0, read it back, close. */
export function freeTcpPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const srv = net.createServer();
    srv.unref();
    srv.on("error", reject);
    srv.listen(0, "127.0.0.1", () => {
      const addr = srv.address();
      if (addr && typeof addr === "object") {
        const port = addr.port;
        srv.close(() => resolve(port));
      } else {
        srv.close();
        reject(new Error("could not read back bound port"));
      }
    });
  });
}

/** A free UDP port for SRT, which binds its own datagram socket (not TCP). */
export function freeUdpPort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const sock = dgram.createSocket("udp4");
    sock.unref();
    sock.on("error", reject);
    sock.bind(0, "127.0.0.1", () => {
      const port = sock.address().port;
      sock.close(() => resolve(port));
    });
  });
}

export interface CaudalServer {
  httpPort: number;
  rtmpPort: number;
  srtPort: number;
  baseUrl: string;
  rtmpUrl(streamName: string): string;
  stop(): Promise<void>;
}

function resolveBinary(): string {
  const fromEnv = process.env.CAUDAL_BIN;
  const path = fromEnv && fromEnv.length > 0 ? fromEnv : join(__dirname, "..", "..", "..", "target", "release", "caudal");
  return path;
}

/**
 * Starts the real `caudal` binary against a temp config with free ports,
 * and waits for `GET /healthz` == 200. Throws a clear, actionable error if
 * the binary is missing (it never builds it itself).
 */
export async function startCaudal(): Promise<CaudalServer> {
  const bin = resolveBinary();
  const fs = await import("node:fs");
  if (!fs.existsSync(bin)) {
    throw new Error(
      `caudal binary not found at "${bin}".\n` +
        `Build it first: cargo build --release -p caudal\n` +
        `(or set CAUDAL_BIN to point at an existing build).`,
    );
  }

  const [httpPort, rtmpPort, srtPort] = await Promise.all([freeTcpPort(), freeTcpPort(), freeUdpPort()]);

  const dir = await mkdtemp(join(tmpdir(), "caudal-browser-"));
  const cfgPath = join(dir, "caudal.toml");
  const toml = [
    "[server]",
    `http_bind = "127.0.0.1:${httpPort}"`,
    "",
    "[rtmp]",
    `bind = "127.0.0.1:${rtmpPort}"`,
    `app = "live"`,
    "",
    "[srt]",
    `bind = "127.0.0.1:${srtPort}"`,
    "",
    "[hls]",
    "part_ms = 200",
    "segment_ms = 2000",
    "",
  ].join("\n");
  await writeFile(cfgPath, toml, "utf8");

  const child: ChildProcess = spawn(bin, ["--config", cfgPath], {
    stdio: ["ignore", "ignore", "inherit"],
  });

  let exited = false;
  child.on("exit", () => {
    exited = true;
  });

  const baseUrl = `http://127.0.0.1:${httpPort}`;

  const deadline = Date.now() + 10_000;
  let lastErr: unknown;
  while (Date.now() < deadline) {
    if (exited) {
      throw new Error("caudal exited before /healthz answered — check its stderr above");
    }
    try {
      const res = await fetch(`${baseUrl}/healthz`);
      if (res.status === 200) {
        return {
          httpPort,
          rtmpPort,
          srtPort,
          baseUrl,
          rtmpUrl: (name: string) => `rtmp://127.0.0.1:${rtmpPort}/live/${name}`,
          stop: async () => {
            child.kill("SIGTERM");
            await new Promise<void>((resolve) => {
              if (exited) return resolve();
              child.once("exit", () => resolve());
              setTimeout(() => {
                child.kill("SIGKILL");
                resolve();
              }, 3000);
            });
            await rm(dir, { recursive: true, force: true });
          },
        };
      }
    } catch (e) {
      lastErr = e;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  child.kill("SIGKILL");
  await rm(dir, { recursive: true, force: true });
  throw new Error(`caudal never answered 200 on /healthz within 10s (last error: ${String(lastErr)})`);
}

export interface FfmpegPublisher {
  stop(): Promise<void>;
}

/**
 * Publishes a synthetic 1280x720 H.264 + AAC test pattern over RTMP in real
 * time, matching the Rust harness's `Publisher::rtmp` (minus the burned-in
 * clock, which this suite does not read).
 */
export function startFfmpegPublisher(rtmpUrl: string): FfmpegPublisher {
  const args = [
    "-hide_banner",
    "-loglevel",
    "error",
    "-re",
    "-f",
    "lavfi",
    "-i",
    "testsrc2=size=1280x720:rate=30",
    "-f",
    "lavfi",
    "-i",
    "sine=frequency=440:sample_rate=48000",
    "-c:v",
    "libx264",
    "-preset",
    "veryfast",
    "-tune",
    "zerolatency",
    "-g",
    "60",
    "-b:v",
    "2M",
    "-c:a",
    "aac",
    "-f",
    "flv",
    rtmpUrl,
  ];
  const child = spawn("ffmpeg", args, { stdio: ["ignore", "ignore", "inherit"] });
  return {
    stop: async () => {
      child.kill("SIGTERM");
      await new Promise<void>((resolve) => {
        child.once("exit", () => resolve());
        setTimeout(() => {
          child.kill("SIGKILL");
          resolve();
        }, 2000);
      });
    },
  };
}

export async function ensureResultsDir(): Promise<string> {
  const dir = join(__dirname, "..", "results");
  await mkdir(dir, { recursive: true });
  return dir;
}
