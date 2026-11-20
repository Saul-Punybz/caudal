//! The reusable parts of an ffmpeg pipe, shared by the `Ffmpeg` engine and
//! by other crates that feed ffmpeg on stdin and read MPEG-TS back (e.g.
//! `caudal-omt`'s raw-frame feed):
//!
//! - [`FfmpegProcess`]: spawn in its own process group, stderr logged at
//!   `debug` with the last lines kept for a crash report, whole group
//!   SIGKILLed on shutdown or drop.
//! - [`read_ts_outputs`]: ffmpeg's single MPEG-TS on stdout, split by PID
//!   into one `caudal-ts` demuxer per output stream, published through
//!   [`RenditionOut`] on the caller's clock ([`Clock`]).
//! - [`MkvWriter`]: a minimal live Matroska writer for ffmpeg's stdin
//!   (coded video, Opus, raw video, float PCM).

use std::collections::VecDeque;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use caudal_ts::demux::{DemuxEvent, Demuxer};
use caudal_ts::ts::{EsKind, TsDemux};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::task::JoinHandle;

pub use crate::mkv::{MkvTrack, MkvWriter};
pub use crate::out::{AUDIO_OUT, Clock, HEADROOM_US, RenditionOut, VIDEO_OUT};

/// First PID of ffmpeg's output streams (`-mpegts_start_pid`): output
/// stream `k` (in `-map` order) is on PID `START_PID + k`.
pub const START_PID: u16 = 0x100;
const PMT_PID: u16 = 0x1000;
const TS_PACKET: usize = 188;
/// stderr lines kept for the crash report.
const TAIL_LINES: usize = 8;

/// ffmpeg's process group, killed whole when dropped.
struct ProcGuard {
    child: Child,
}

impl ProcGuard {
    fn kill_group(&mut self) {
        // `id()` is `None` once the child was reaped, so a recycled pid is
        // never signalled. A direct killpg: `/bin/kill -KILL -<pgid>` from
        // Linux procps signals every process of the user instead (it killed
        // the GitHub runner, and would kill everything Caudal's user runs).
        if let Some(pid) = self.child.id().and_then(|p| rustix::process::Pid::from_raw(p as i32)) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.start_kill();
    }
}

impl Drop for ProcGuard {
    fn drop(&mut self) {
        self.kill_group();
    }
}

/// A running ffmpeg (or any filter process) with piped stdin/stdout. The
/// whole process group is killed when this is dropped.
pub struct FfmpegProcess {
    guard: ProcGuard,
    tail: Arc<Mutex<VecDeque<String>>>,
    stderr_task: JoinHandle<()>,
}

impl FfmpegProcess {
    /// Spawns `program args` in a new process group, stdin and stdout piped
    /// (returned), stderr logged at `debug` tagged with `stream`. Must be
    /// called inside a tokio runtime.
    pub fn spawn(program: &Path, args: &[String], stream: &str) -> std::io::Result<(Self, ChildStdin, ChildStdout)> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        let mut child = cmd.spawn()?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            // Dropping the guard kills it.
            drop(ProcGuard { child });
            return Err(std::io::Error::other("ffmpeg pipes missing"));
        };
        let tail: Arc<Mutex<VecDeque<String>>> = Arc::default();
        let stderr_task = {
            let (tail, name) = (tail.clone(), stream.to_owned());
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(stream = %name, "ffmpeg: {line}");
                    let mut t = tail.lock().expect("poisoned");
                    if t.len() == TAIL_LINES {
                        t.pop_front();
                    }
                    t.push_back(line);
                }
            })
        };
        Ok((Self { guard: ProcGuard { child }, tail, stderr_task }, stdin, stdout))
    }

    /// The OS process id, until the process is reaped.
    pub fn id(&self) -> Option<u32> {
        self.guard.child.id()
    }

    /// Kills the group, reaps the child, lets the stderr logger finish
    /// (up to 500 ms) and returns the last stderr lines.
    pub async fn shutdown(mut self) -> Vec<String> {
        self.guard.kill_group();
        let _ = tokio::time::timeout(Duration::from_secs(2), self.guard.child.wait()).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut self.stderr_task).await;
        self.stderr_task.abort();
        self.tail.lock().expect("poisoned").iter().cloned().collect()
    }
}

impl Drop for FfmpegProcess {
    fn drop(&mut self) {
        self.stderr_task.abort();
    }
}

/// Per-output demux state.
struct OutDemux {
    ts: TsDemux,
    demux: Demuxer,
    /// The raw 90 kHz timestamp the `Demuxer` rebased to zero (its first
    /// unit's), needed to undo that rebase.
    zero: Option<i64>,
}

/// Reads ffmpeg's single MPEG-TS from `stdout`, splits it by PID into `n`
/// outputs of `per` elementary streams each (PIDs from [`START_PID`], in
/// `-map` order; PAT/PMT go to all), and pushes each output's frames into
/// `outs[i]` shifted back onto the source clock with `clock`. Returns at
/// the end of ffmpeg's output.
pub async fn read_ts_outputs(
    mut stdout: ChildStdout,
    outs: Arc<Mutex<Vec<RenditionOut>>>,
    clock: Clock,
    per: usize,
    n: usize,
) {
    let per = per.max(1);
    let mut demux: Vec<OutDemux> =
        (0..n).map(|_| OutDemux { ts: TsDemux::new(), demux: Demuxer::new(), zero: None }).collect();
    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    let mut units = Vec::new();
    let mut events = Vec::new();
    let video_off = clock.back_offset(90_000);
    loop {
        let n_read = match stdout.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(k) => k,
        };
        carry.extend_from_slice(&buf[..n_read]);
        let mut pos = 0;
        while carry.len() - pos >= TS_PACKET {
            if carry[pos] != 0x47 {
                pos += 1; // resync on the sync byte
                continue;
            }
            let pkt = &carry[pos..pos + TS_PACKET];
            let pid = (u16::from(pkt[1] & 0x1F) << 8) | u16::from(pkt[2]);
            if pid == 0 || pid == PMT_PID {
                for d in demux.iter_mut() {
                    d.ts.feed(pkt);
                }
            } else if pid >= START_PID {
                let idx = usize::from(pid - START_PID) / per;
                if let Some(d) = demux.get_mut(idx) {
                    d.ts.feed(pkt);
                }
            }
            pos += TS_PACKET;
        }
        carry.drain(..pos);

        let mut outs = outs.lock().expect("poisoned");
        for (d, out) in demux.iter_mut().zip(outs.iter_mut()) {
            d.ts.drain(&mut units);
            for unit in units.drain(..) {
                if d.zero.is_none() {
                    let raw = match unit.kind {
                        EsKind::Aac => unit.pts.or(unit.dts),
                        _ => unit.dts.or(unit.pts),
                    };
                    d.zero = Some(raw.unwrap_or(0) as i64);
                }
                d.demux.consume(unit, &mut events);
            }
            let zero = d.zero.unwrap_or(0);
            for ev in events.drain(..) {
                match ev {
                    DemuxEvent::VideoInit(info) | DemuxEvent::AudioInit(info) => out.set_track(info),
                    DemuxEvent::VideoFrame(mut f) => {
                        f.track = VIDEO_OUT;
                        f.dts += zero + video_off;
                        f.pts += zero + video_off;
                        out.push(f);
                    }
                    DemuxEvent::AudioFrame(mut f) => {
                        let rate = out.audio_rate().unwrap_or(48_000);
                        let off = (i128::from(zero) * i128::from(rate) / 90_000) as i64 + clock.back_offset(rate);
                        f.track = AUDIO_OUT;
                        f.dts += off;
                        f.pts += off;
                        out.push(f);
                    }
                    DemuxEvent::Cue(_) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod kill_tests {
    use super::ProcGuard;
    use std::time::Duration;
    use tokio::process::Command;

    /// Dropping a guard kills its own process group and nothing else. On
    /// Linux, the old `/bin/kill -KILL -<pgid>` killed every process of the
    /// user, the bystander below included (and the GitHub runner with it).
    #[tokio::test]
    async fn kills_its_group_and_spares_everyone_else() {
        let spawn = || Command::new("sleep").arg("30").process_group(0).kill_on_drop(true).spawn().unwrap();
        let mut bystander = spawn();
        let mut guard = ProcGuard { child: spawn() };
        let pid = guard.child.id().unwrap();
        guard.kill_group();
        let status = tokio::time::timeout(Duration::from_secs(5), guard.child.wait()).await.unwrap().unwrap();
        assert!(!status.success(), "sleep {pid} was not killed");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(bystander.try_wait().unwrap().is_none(), "a process outside the group was killed");
        let _ = bystander.start_kill();
    }
}
