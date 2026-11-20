//! `[[udp.pull]]` against a real sender: ffmpeg sending MPEG-TS over UDP
//! (unicast, RTP, multicast), the way an encoder or VLC feeds a playout
//! chain.

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use caudal_core::{BufferConfig, Codec, Registry, Stream};
use caudal_udp::{UdpPull, start_pulls};

fn have(bin: &str) -> bool {
    Command::new("which").arg(bin).output().is_ok_and(|o| o.status.success())
}

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// ffmpeg in its own process group, killed with it on drop.
struct Sender(Child);

impl Sender {
    fn start(format: &str, url: &str) -> Self {
        use std::os::unix::process::CommandExt;
        let child = Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-re", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000", "-c:v", "libx264", "-g", "25"])
            .args(["-c:a", "aac", "-t", "60", "-f", format, url])
            .process_group(0)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn ffmpeg");
        Self(child)
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        if let Some(pid) = rustix_pid(self.0.id()) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn rustix_pid(pid: u32) -> Option<rustix::process::Pid> {
    rustix::process::Pid::from_raw(pid as i32)
}

async fn wait_for<T>(timeout: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(v) = f() {
            return Some(v);
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn assert_live_h264_aac(registry: &Registry, name: &str) -> Arc<Stream> {
    let stream = wait_for(Duration::from_secs(15), || registry.get(name)).await.expect("published");
    let codecs = wait_for(Duration::from_secs(10), || {
        let c: Vec<Codec> = stream.tracks().iter().map(|t| t.codec).collect();
        (c.contains(&Codec::H264) && c.contains(&Codec::Aac)).then_some(c)
    })
    .await;
    assert!(codecs.is_some(), "H.264 + AAC tracks: {:?}", stream.tracks().iter().map(|t| t.codec).collect::<Vec<_>>());
    let before = stream.stats().frames_in;
    assert!(
        wait_for(Duration::from_secs(5), || (stream.stats().frames_in > before + 25).then_some(())).await.is_some(),
        "frames keep arriving"
    );
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_ts_over_unicast_udp_and_the_idle_end() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let port = free_udp_port();
    let registry = Registry::new();
    let pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![UdpPull { stream: "udp1".into(), url: format!("udp://127.0.0.1:{port}"), interface: None }],
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sender = Sender::start("mpegts", &format!("udp://127.0.0.1:{port}?pkt_size=1316"));
    let stream = assert_live_h264_aac(&registry, "udp1").await;

    // The sender stops: after IDLE without a datagram the stream ends.
    drop(sender);
    let ended = wait_for(caudal_udp::IDLE + Duration::from_secs(3), || stream.is_ended().then_some(())).await;
    assert!(ended.is_some(), "ended after {:?} of silence", caudal_udp::IDLE);

    // It starts again: the next datagram publishes the name anew.
    let _sender = Sender::start("mpegts", &format!("udp://127.0.0.1:{port}?pkt_size=1316"));
    let again = wait_for(Duration::from_secs(15), || registry.get("udp1").filter(|s| !s.is_ended())).await;
    assert!(again.is_some_and(|s| !Arc::ptr_eq(&s, &stream)), "republished");
    pulls.reload(&registry, Vec::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_in_rtp() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let port = free_udp_port();
    let registry = Registry::new();
    let _pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![UdpPull { stream: "rtp1".into(), url: format!("rtp://127.0.0.1:{port}"), interface: None }],
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _sender = Sender::start("rtp_mpegts", &format!("rtp://127.0.0.1:{port}"));
    assert_live_h264_aac(&registry, "rtp1").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ts_over_multicast() {
    if !have("ffmpeg") {
        eprintln!("SKIP: ffmpeg not installed");
        return;
    }
    let port = free_udp_port();
    // An administratively scoped group, TTL 0: never leaves this machine.
    // Sender and receiver both use the default multicast interface, like
    // caudal-multicast's loopback test: on Linux a join on the default
    // interface does not see what is sent out of `lo` (localaddr=127.0.0.1),
    // although macOS delivers it.
    let group = format!("239.255.{}.{}", port % 250, port / 250 % 250);
    let registry = Registry::new();
    let _pulls = start_pulls(
        registry.clone(),
        BufferConfig::default(),
        vec![UdpPull { stream: "mc1".into(), url: format!("udp://{group}:{port}"), interface: None }],
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _sender = Sender::start("mpegts", &format!("udp://{group}:{port}?pkt_size=1316&ttl=0"));
    assert_live_h264_aac(&registry, "mc1").await;
}
