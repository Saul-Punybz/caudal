//! Builds the DESCRIBE response body: an SDP session description with one
//! media section per track, control URLs the server's own SETUP handler
//! understands (`.../streamid=<id>`).

use std::io::Cursor;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use bytes::Bytes;
use caudal_core::{Codec, TrackInfo, TrackKind};
use sdp_types::{AddrType, Connection, Media, MediaType, NetType, Origin, Session, Time, TransportProto};

use crate::rtp::{AUDIO_PT, VIDEO_PT};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn control_url(host: &str, stream: &str, track_id: u32, query: &str) -> String {
    format!("rtsp://{host}/{stream}/streamid={track_id}{query}")
}

/// `profile-level-id` + `sprop-parameter-sets` from an avcC blob.
fn h264_fmtp(init: &[u8]) -> Option<String> {
    let record =
        scuffle_h264::AVCDecoderConfigurationRecord::parse(&mut Cursor::new(Bytes::copy_from_slice(init))).ok()?;
    let profile_level =
        format!("{:02X}{:02X}{:02X}", record.profile_indication, record.profile_compatibility, record.level_indication);
    let sprop = record.sps.iter().chain(record.pps.iter()).map(|nal| BASE64.encode(nal)).collect::<Vec<_>>().join(",");
    Some(format!("{VIDEO_PT} packetization-mode=1;profile-level-id={profile_level};sprop-parameter-sets={sprop}"))
}

/// `sprop-vps`/`sprop-sps`/`sprop-pps` from an hvcC blob.
fn h265_fmtp(init: &[u8]) -> Option<String> {
    let record = scuffle_h265::HEVCDecoderConfigurationRecord::demux(Cursor::new(Bytes::copy_from_slice(init))).ok()?;
    let find = |ty| record.arrays.iter().find(|a| a.nal_unit_type == ty).and_then(|a| a.nalus.first());
    let vps = find(scuffle_h265::NALUnitType::VpsNut)?;
    let sps = find(scuffle_h265::NALUnitType::SpsNut)?;
    let pps = find(scuffle_h265::NALUnitType::PpsNut)?;
    Some(format!(
        "{VIDEO_PT} sprop-vps={};sprop-sps={};sprop-pps={}",
        BASE64.encode(vps),
        BASE64.encode(sps),
        BASE64.encode(pps)
    ))
}

pub(crate) fn build(stream: &str, host: &str, tracks: &[TrackInfo], token: Option<&str>) -> Vec<u8> {
    let origin = Origin::new("-", 0u64, NetType::In, AddrType::Ip4, "0.0.0.0");
    let mut session = Session::new(origin, stream.to_owned());
    session.set_connection(Connection::new(NetType::In, AddrType::Ip4, "0.0.0.0"));
    session.add_time(Time::new(0, 0));
    session.add_attribute_with_value("control", "*");

    let query = token.map(|t| format!("?token={t}")).unwrap_or_default();

    for t in tracks {
        match t.kind() {
            TrackKind::Video => {
                let (rtpmap, fmtp) = match t.codec {
                    Codec::H264 => (format!("{VIDEO_PT} H264/90000"), h264_fmtp(&t.init)),
                    Codec::H265 => (format!("{VIDEO_PT} H265/90000"), h265_fmtp(&t.init)),
                    _ => continue,
                };
                let mut m = Media::new(MediaType::Video, 0, TransportProto::RtpAvp, VIDEO_PT.to_string());
                m.add_attribute_with_value("rtpmap", rtpmap);
                if let Some(fmtp) = fmtp {
                    m.add_attribute_with_value("fmtp", fmtp);
                }
                m.add_attribute_with_value("control", control_url(host, stream, t.id.0, &query));
                session.add_media(m);
            }
            TrackKind::Audio => {
                if t.codec != Codec::Aac {
                    continue;
                }
                let sample_rate = t.audio.map_or(t.timescale, |a| a.sample_rate);
                let channels = t.audio.map_or(2, |a| a.channels);
                let rtpmap = format!("{AUDIO_PT} mpeg4-generic/{sample_rate}/{channels}");
                let fmtp = format!(
                    "{AUDIO_PT} streamtype=5;profile-level-id=1;mode=AAC-hbr;\
                     sizelength=13;indexlength=3;indexdeltalength=3;config={}",
                    hex(&t.init)
                );
                let mut m = Media::new(MediaType::Audio, 0, TransportProto::RtpAvp, AUDIO_PT.to_string());
                m.add_attribute_with_value("rtpmap", rtpmap);
                m.add_attribute_with_value("fmtp", fmtp);
                m.add_attribute_with_value("control", control_url(host, stream, t.id.0, &query));
                session.add_media(m);
            }
            _ => {}
        }
    }

    let mut buf = Vec::new();
    session.write(&mut buf).expect("writing SDP to a Vec never fails");
    buf
}
