//! Audio-only HTTP output: a stream's audio as one endless HTTP response,
//! the way an internet radio station serves it. Any player opens the URL:
//! VLC, a browser `<audio>`, a smart speaker, an internet radio app.
//!
//! - `GET /audio/{name}.aac`: ADTS AAC (`audio/aac`), for AAC sources (what
//!   RTMP and most SRT encoders send).
//! - `GET /audio/{name}.mp3`: MPEG audio (`audio/mpeg`), for MP3 and MP2
//!   sources (broadcast feeds carry MP2; Layer II frames are valid in an
//!   `audio/mpeg` stream and every player that reads MP3 reads them).
//!
//! The source audio passes through untouched: AAC access units get a 7-byte
//! ADTS header each, MPEG audio frames already carry their own header. No
//! transcoding: a stream whose audio is another codec (Opus, AC-3, ...) or
//! the other one of the two gets `415` naming the URL that does work. MP3
//! transcoding stays out on purpose: it would cost one ffmpeg per stream
//! for what an operator can fix once at the encoder.
//!
//! A response starts at the live edge, stays open while the stream is live
//! and ends when the stream ends. A client that falls more than
//! [`MAX_BEHIND`] behind live is moved to the live edge (dropping what it
//! missed), so a slow listener never holds more than the shared stream
//! buffer. Each response is a viewer session (protocol `audio`) that an
//! operator can kick. With `Icy-MetaData: 1` the response carries SHOUTcast
//! style metadata (`icy-metaint`) with the stream name as the title.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Path, RawQuery, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::{BufMut, Bytes, BytesMut};
use caudal_core::media::valid_stream_name;
use caudal_core::{Access, Cidr, Codec, Denied, Event, Registry, Session, StartAt, Subscriber, TrackInfo, TrackKind};

/// A listener this far behind the newest frame is moved to the live edge.
pub const MAX_BEHIND: Duration = Duration::from_secs(5);

/// How long a request for a stream that has not announced its tracks yet
/// waits for them.
const TRACK_WAIT: Duration = Duration::from_secs(5);

/// Audio bytes between two ICY metadata blocks (SHOUTcast's own default).
pub const ICY_METAINT: usize = 16_000;

struct Shared {
    registry: Arc<Registry>,
    trusted_proxies: Vec<Cidr>,
}

/// Serves `/audio/{name}.aac` and `/audio/{name}.mp3`. Mounted at the root
/// by the server; paths are absolute.
///
/// `trusted_proxies` is `[server] trusted_proxies`: reverse proxies allowed
/// to set `X-Forwarded-For` for the address a play request is judged by.
pub fn router(registry: Arc<Registry>, trusted_proxies: Vec<Cidr>) -> Router {
    Router::new().route("/audio/{file}", get(audio)).with_state(Arc::new(Shared { registry, trusted_proxies }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Aac,
    Mpeg,
}

impl Format {
    fn from_ext(ext: &str) -> Option<Self> {
        match ext {
            "aac" => Some(Format::Aac),
            "mp3" => Some(Format::Mpeg),
            _ => None,
        }
    }

    fn ext(self) -> &'static str {
        match self {
            Format::Aac => "aac",
            Format::Mpeg => "mp3",
        }
    }

    fn content_type(self) -> &'static str {
        match self {
            Format::Aac => "audio/aac",
            Format::Mpeg => "audio/mpeg",
        }
    }

    fn for_codec(codec: Codec) -> Option<Self> {
        match codec {
            Codec::Aac => Some(Format::Aac),
            Codec::Mp3 | Codec::Mp2 => Some(Format::Mpeg),
            _ => None,
        }
    }
}

fn text(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], msg.into()).into_response()
}

/// `?token=` wins over `Authorization: Bearer`, as on every playback route.
fn request_token<'a>(query: &'a str, req: &'a Request) -> Option<&'a str> {
    let from_query = query.split('&').find_map(|kv| kv.strip_prefix("token=")).filter(|t| !t.is_empty());
    from_query.or_else(|| {
        req.headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty())
    })
}

/// JWT alphabet only, as on the other playback routes.
fn token_is_url_safe(t: &str) -> bool {
    t.len() <= 4096 && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The TCP peer, or (only if it is a trusted proxy) the right-most
/// untrusted `X-Forwarded-For` hop. `None` without connect info (tests).
fn client_ip(req: &Request, trusted_proxies: &[Cidr]) -> Option<IpAddr> {
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>()?.0.ip();
    let xff = req.headers().get("x-forwarded-for").and_then(|v| v.to_str().ok());
    Some(caudal_core::net::resolve_forwarded(peer, xff, trusted_proxies))
}

fn denied(d: Denied) -> Response {
    match d {
        Denied::Missing => {
            let mut r = text(StatusCode::UNAUTHORIZED, "token required");
            r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            r
        }
        Denied::Refused(_) => text(StatusCode::FORBIDDEN, "forbidden"),
    }
}

async fn audio(
    State(st): State<Arc<Shared>>,
    Path(file): Path<String>,
    RawQuery(q): RawQuery,
    req: Request,
) -> Response {
    let not_found = || text(StatusCode::NOT_FOUND, "no such stream");
    let Some((name, ext)) = file.rsplit_once('.') else { return not_found() };
    let Some(format) = Format::from_ext(ext) else { return not_found() };
    if !valid_stream_name(name) {
        return not_found();
    }
    let q = q.unwrap_or_default();
    let token = request_token(&q, &req);
    if token.is_some_and(|t| !token_is_url_safe(t)) {
        return text(StatusCode::FORBIDDEN, "bad token");
    }
    let ip = client_ip(&req, &st.trusted_proxies);
    // Like HLS: a name nobody publishes is a 404 whatever the gate says,
    // and only an authorized request may start a cluster pull.
    let known = st.registry.get(name).is_some();
    if let Err(d) = st.registry.authorize(Access::Play, name, token, ip).await {
        return if known { denied(d) } else { not_found() };
    }
    let Some(stream) = st.registry.get_or_demand(name).await else { return not_found() };

    let mut sub = stream.subscribe(StartAt::LiveEdge);
    let Some(track) = audio_track(&mut sub).await else {
        return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "this stream has no audio track");
    };
    if Format::for_codec(track.codec) != Some(format) {
        let msg = match Format::for_codec(track.codec) {
            Some(other) => format!(
                "this stream's audio is {}, not served as .{}; use /audio/{name}.{}",
                track.codec.as_str(),
                format.ext(),
                other.ext()
            ),
            None => format!("this stream's audio is {}; only AAC, MP3 and MP2 are served", track.codec.as_str()),
        };
        return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, msg);
    }
    let adts = match format {
        Format::Aac => match Adts::from_asc(&track.init) {
            Some(a) => Some(a),
            None => {
                return text(StatusCode::UNSUPPORTED_MEDIA_TYPE, "this stream's AAC configuration has no ADTS form");
            }
        },
        Format::Mpeg => None,
    };
    let icy =
        req.headers().get("icy-metadata").is_some_and(|v| v.as_bytes().trim_ascii() == b"1").then(|| Icy::new(name));

    let mut resp = Response::new(Body::empty());
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(format.content_type()));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-store"));
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    if icy.is_some() {
        h.insert("icy-metaint", HeaderValue::from(ICY_METAINT));
        if let Ok(v) = HeaderValue::from_str(name) {
            h.insert("icy-name", v);
        }
    }
    let session = st.registry.open_session(name, "audio", ip);
    tracing::debug!(stream = %name, codec = track.codec.as_str(), session = session.id(), "audio listener");
    let out = Output { sub, session, format, track, adts, icy };
    *resp.body_mut() = Body::from_stream(futures_util::stream::unfold(out, |mut out| async move {
        let chunk = out.next_chunk().await?;
        Some((Ok::<_, Infallible>(chunk), out))
    }));
    resp
}

/// The stream's first audio track. Waits up to [`TRACK_WAIT`] for a stream
/// that has not announced any tracks yet.
async fn audio_track(sub: &mut Subscriber) -> Option<TrackInfo> {
    let deadline = tokio::time::Instant::now() + TRACK_WAIT;
    loop {
        let tracks = sub.tracks();
        if !tracks.is_empty() {
            return tracks.into_iter().find(|t| t.kind() == TrackKind::Audio);
        }
        match tokio::time::timeout_at(deadline, sub.recv()).await {
            Ok(Event::End) | Err(_) => return None,
            Ok(_) => {}
        }
    }
}

/// One listener's response body. Dropped (closing the subscriber and the
/// session) when the client goes away.
struct Output {
    sub: Subscriber,
    session: Session,
    format: Format,
    track: TrackInfo,
    adts: Option<Adts>,
    icy: Option<Icy>,
}

impl Output {
    /// The next bytes to send, or `None` to end the body.
    async fn next_chunk(&mut self) -> Option<Bytes> {
        loop {
            let ev = tokio::select! {
                ev = self.sub.recv() => ev,
                _ = self.session.kicked() => return None,
            };
            match ev {
                Event::Frame(f) if f.track == self.track.id => {
                    if let Some(newest) = self.sub.stream().newest_micros()
                        && newest - self.track.to_micros(f.dts) > MAX_BEHIND.as_micros() as i64
                    {
                        let skipped = self.sub.skip_to_live();
                        tracing::debug!(stream = %self.sub.stream().name(), skipped, "audio listener behind; resynced to live");
                        continue;
                    }
                    let mut buf = BytesMut::with_capacity(f.data.len() + 7);
                    if let Some(adts) = &self.adts {
                        buf.put_slice(&adts.header(f.data.len()));
                    }
                    buf.put_slice(&f.data);
                    let out = match &mut self.icy {
                        Some(icy) => icy.wrap(&buf),
                        None => buf.freeze(),
                    };
                    self.session.add_bytes(out.len());
                    return Some(out);
                }
                Event::TracksChanged => {
                    // Keep going on any audio track the same format can
                    // carry; anything else ends the response, and the
                    // player's reconnect gets a clear answer.
                    let next = self.sub.tracks().into_iter().find(|t| t.kind() == TrackKind::Audio)?;
                    if Format::for_codec(next.codec) != Some(self.format) {
                        return None;
                    }
                    if self.format == Format::Aac {
                        self.adts = Some(Adts::from_asc(&next.init)?);
                    }
                    self.track = next;
                }
                Event::End => return None,
                Event::Frame(_) | Event::Lagged { .. } | Event::Cue(_) => {}
            }
        }
    }
}

/// The fixed ADTS fields, from a track's `AudioSpecificConfig`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Adts {
    /// `audioObjectType - 1`.
    profile: u8,
    sampling_freq_idx: u8,
    channel_config: u8,
}

impl Adts {
    /// `None` for configurations ADTS cannot describe: an escaped object
    /// type, an explicit sample rate, or channels set by a PCE.
    fn from_asc(asc: &[u8]) -> Option<Self> {
        let [a, b, ..] = *asc else { return None };
        let aot = a >> 3;
        let sampling_freq_idx = ((a & 0x07) << 1) | (b >> 7);
        let channel_config = (b >> 3) & 0x0F;
        let profile = match aot {
            1..=4 => aot - 1,
            // HE-AAC v1/v2 signalled explicitly: the first index is the core
            // rate, and ADTS carries the LC core (SBR/PS are implicit).
            5 | 29 => 1,
            _ => return None,
        };
        if sampling_freq_idx > 12 || channel_config == 0 || channel_config > 7 {
            return None;
        }
        Some(Self { profile, sampling_freq_idx, channel_config })
    }

    /// A 7-byte header (no CRC) for one raw access unit.
    fn header(&self, payload_len: usize) -> [u8; 7] {
        let len = (7 + payload_len).min(0x1FFF) as u16;
        let (p, s, c) = (self.profile, self.sampling_freq_idx, self.channel_config);
        [
            0xFF,
            0xF1,
            (p << 6) | (s << 2) | ((c >> 2) & 0x01),
            ((c & 0x03) << 6) | ((len >> 11) & 0x03) as u8,
            ((len >> 3) & 0xFF) as u8,
            (((len & 0x07) as u8) << 5) | 0x1F,
            0xFC,
        ]
    }
}

/// SHOUTcast metadata interleaving: after every [`ICY_METAINT`] audio
/// bytes, one length byte (`n`) and `n * 16` bytes of metadata. The title
/// goes out in the first block; later blocks are empty (`n = 0`, "no
/// change").
struct Icy {
    until_meta: usize,
    title: Option<Vec<u8>>,
}

impl Icy {
    fn new(name: &str) -> Self {
        // Stream names are `[A-Za-z0-9_.+-]`: nothing to escape.
        let mut meta = format!("StreamTitle='{name}';").into_bytes();
        meta.resize(meta.len().div_ceil(16) * 16, 0);
        Self { until_meta: ICY_METAINT, title: Some(meta) }
    }

    fn wrap(&mut self, mut audio: &[u8]) -> Bytes {
        let mut out = BytesMut::with_capacity(audio.len() + 64);
        while !audio.is_empty() {
            let n = self.until_meta.min(audio.len());
            out.put_slice(&audio[..n]);
            audio = &audio[n..];
            self.until_meta -= n;
            if self.until_meta == 0 {
                match self.title.take() {
                    Some(t) => {
                        out.put_u8((t.len() / 16) as u8);
                        out.put_slice(&t);
                    }
                    None => out.put_u8(0),
                }
                self.until_meta = ICY_METAINT;
            }
        }
        out.freeze()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adts_from_asc() {
        // AAC-LC, 48 kHz (index 3), stereo.
        let a = Adts::from_asc(&[0x11, 0x90]).unwrap();
        assert_eq!(a, Adts { profile: 1, sampling_freq_idx: 3, channel_config: 2 });
        let h = a.header(100);
        assert_eq!(&h[..2], &[0xFF, 0xF1]);
        let len = ((u16::from(h[3]) & 0x03) << 11) | (u16::from(h[4]) << 3) | (u16::from(h[5]) >> 5);
        assert_eq!(len, 107);
        // HE-AAC explicit (aot 5), 24 kHz core, stereo: carried as LC.
        assert_eq!(Adts::from_asc(&[0x2B, 0x10, 0x88, 0x00]).map(|a| a.profile), Some(1));
        // Channels from a PCE, explicit rate, escaped object type, too short.
        assert_eq!(Adts::from_asc(&[0x11, 0x80]), None);
        assert_eq!(Adts::from_asc(&[0x17, 0x80]), None);
        assert_eq!(Adts::from_asc(&[0xF8, 0x00]), None);
        assert_eq!(Adts::from_asc(&[0x11]), None);
    }

    #[test]
    fn icy_interleaves_title_then_empty_blocks() {
        let mut icy = Icy::new("oceano");
        let audio = vec![0xAAu8; ICY_METAINT * 2 + 10];
        let mut out = Vec::new();
        // Odd chunk sizes, as frames arrive.
        for c in audio.chunks(417) {
            out.extend_from_slice(&icy.wrap(c));
        }
        let title = b"StreamTitle='oceano';";
        let n = title.len().div_ceil(16);
        assert!(out[..ICY_METAINT].iter().all(|&b| b == 0xAA));
        assert_eq!(out[ICY_METAINT] as usize, n);
        let meta = &out[ICY_METAINT + 1..ICY_METAINT + 1 + n * 16];
        assert_eq!(&meta[..title.len()], title);
        assert!(meta[title.len()..].iter().all(|&b| b == 0));
        let rest = &out[ICY_METAINT + 1 + n * 16..];
        assert!(rest[..ICY_METAINT].iter().all(|&b| b == 0xAA));
        assert_eq!(rest[ICY_METAINT], 0, "later blocks are empty");
        assert_eq!(rest.len(), ICY_METAINT + 1 + 10);
    }

    #[test]
    fn formats() {
        assert_eq!(Format::for_codec(Codec::Mp2), Some(Format::Mpeg));
        assert_eq!(Format::for_codec(Codec::Mp3), Some(Format::Mpeg));
        assert_eq!(Format::for_codec(Codec::Aac), Some(Format::Aac));
        assert_eq!(Format::for_codec(Codec::Opus), None);
        assert_eq!(Format::from_ext("wav"), None);
    }
}
