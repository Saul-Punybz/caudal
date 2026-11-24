//! CMAF boxes: one init segment (`ftyp` + `moov`) and fragments
//! (`moof` + `mdat`) with every track muxed into the same fragment.
//!
//! `mp4-atom` writes the boxes; this file only decides their contents.

use bytes::Bytes;
use caudal_core::{Codec, TrackInfo, TrackKind};
use mp4_atom::{
    Atom, Audio, Avc1, Avcc, Decode, Dinf, Dops, Dref, Encode, Esds, Ftyp, Hdlr, Hvc1, Hvcc, Mdhd, Mdia, Mfhd, Minf,
    Moof, Moov, Mp4a, Mvex, Mvhd, Opus, Smhd, Stbl, Stco, Stsd, Tfdt, Tfhd, Tkhd, Traf, Trak, Trex, Trun, TrunEntry,
    Url, Visual, Vmhd, esds,
};

/// `sample_depends_on = 2` (does not depend on others): a sync sample.
const FLAGS_SYNC: u32 = 0x0200_0000;
/// `sample_depends_on = 1`, `sample_is_non_sync_sample = 1`.
const FLAGS_NON_SYNC: u32 = 0x0101_0000;

/// One sample ready to be written into a fragment.
#[derive(Debug, Clone)]
pub struct Sample {
    /// Decode time on the track's clock, already shifted to be non-negative.
    pub dts: i64,
    /// `pts - dts`.
    pub cts: i32,
    pub dur: u32,
    pub key: bool,
    pub data: Bytes,
}

/// A track as written into the MP4 (`track_id` is the MP4 id, 1-based).
#[derive(Debug, Clone)]
pub struct Mp4Track {
    pub track_id: u32,
    pub info: TrackInfo,
}

/// Builds `ftyp` + `moov` for the given tracks, or `None` if a codec
/// configuration cannot be parsed.
pub fn init_segment(tracks: &[Mp4Track]) -> Option<Bytes> {
    let mut traks = Vec::with_capacity(tracks.len());
    let mut trex = Vec::with_capacity(tracks.len());
    for t in tracks {
        traks.push(trak(t)?);
        trex.push(Trex { track_id: t.track_id, default_sample_description_index: 1, ..Default::default() });
    }
    let ftyp = Ftyp {
        major_brand: b"iso6".into(),
        minor_version: 0,
        compatible_brands: vec![b"iso6".into(), b"cmfc".into(), b"mp41".into()],
    };
    let moov = Moov {
        mvhd: Mvhd {
            timescale: 1000,
            rate: 1.into(),
            volume: 1.into(),
            next_track_id: tracks.iter().map(|t| t.track_id).max().unwrap_or(0) + 1,
            ..Default::default()
        },
        mvex: Some(Mvex { mehd: None, trex }),
        trak: traks,
        ..Default::default()
    };
    let mut buf = Vec::new();
    ftyp.encode(&mut buf).ok()?;
    moov.encode(&mut buf).ok()?;
    Some(buf.into())
}

fn trak(t: &Mp4Track) -> Option<Trak> {
    let info = &t.info;
    let video = info.kind() == TrackKind::Video;
    let (width, height) = info
        .video
        .map(|v| (v.width.min(u16::MAX as u32) as u16, v.height.min(u16::MAX as u32) as u16))
        .unwrap_or((0, 0));
    let codec: mp4_atom::Codec = match info.codec {
        Codec::H264 => Avc1 {
            visual: visual(width, height),
            avcc: Avcc::decode_body(&mut &info.init[..]).ok()?,
            btrt: None,
            colr: None,
            pasp: None,
            taic: None,
            fiel: None,
        }
        .into(),
        Codec::H265 => Hvc1 {
            visual: visual(width, height),
            hvcc: Hvcc::decode_body(&mut &info.init[..]).ok()?,
            lhvc: None,
            btrt: None,
            colr: None,
            pasp: None,
            taic: None,
            fiel: None,
            ccst: None,
        }
        .into(),
        Codec::Aac => {
            if info.init.len() < 2 {
                return None;
            }
            let asc = &info.init;
            let profile = asc[0] >> 3;
            let freq_index = ((asc[0] & 0x07) << 1) | (asc[1] >> 7);
            let chan_conf = (asc[1] >> 3) & 0x0f;
            let channels = info.audio.map(|a| a.channels as u16).filter(|&c| c > 0).unwrap_or(chan_conf.max(1) as u16);
            let rate = info.audio.map(|a| a.sample_rate).unwrap_or(info.timescale);
            Mp4a {
                audio: Audio {
                    data_reference_index: 1,
                    channel_count: channels,
                    sample_size: 16,
                    sample_rate: (rate.min(u16::MAX as u32) as u16).into(),
                },
                esds: Esds {
                    es_desc: esds::EsDescriptor {
                        es_id: t.track_id as u16,
                        dec_config: esds::DecoderConfig {
                            object_type_indication: 0x40,
                            stream_type: 5,
                            up_stream: 0,
                            dec_specific: Some(esds::DecoderSpecific {
                                profile,
                                freq_index,
                                chan_conf,
                                raw: asc.to_vec(),
                            }),
                            ..Default::default()
                        },
                        sl_config: esds::SLConfig {},
                    },
                },
                btrt: None,
                taic: None,
            }
            .into()
        }
        Codec::Opus => {
            let head = crate::opus::parse_opus_head(&info.init)?;
            let channels =
                info.audio.map(|a| a.channels as u16).filter(|&c| c > 0).unwrap_or(u16::from(head.channels)).max(1);
            let rate = info.audio.map(|a| a.sample_rate).unwrap_or(48_000);
            Opus {
                audio: Audio {
                    data_reference_index: 1,
                    channel_count: channels,
                    sample_size: 16,
                    sample_rate: (rate.min(u16::MAX as u32) as u16).into(),
                },
                dops: Dops {
                    output_channel_count: head.channels,
                    pre_skip: head.pre_skip,
                    input_sample_rate: head.input_sample_rate,
                    output_gain: head.output_gain,
                },
                btrt: None,
            }
            .into()
        }
        _ => return None,
    };

    let minf = Minf {
        vmhd: video.then(Vmhd::default),
        smhd: (!video).then(Smhd::default),
        dinf: Dinf { dref: Dref { urls: vec![Url { location: String::new() }] } },
        stbl: Stbl { stsd: Stsd { codecs: vec![codec] }, stco: Some(Stco::default()), ..Default::default() },
        ..Default::default()
    };
    Some(Trak {
        tkhd: Tkhd {
            track_id: t.track_id,
            enabled: true,
            in_movie: true,
            volume: if video { 0.into() } else { 1.into() },
            width: width.into(),
            height: height.into(),
            ..Default::default()
        },
        mdia: Mdia {
            mdhd: Mdhd {
                timescale: info.timescale,
                language: info.lang.clone().filter(|l| l.len() == 3).unwrap_or_else(|| "und".into()),
                ..Default::default()
            },
            hdlr: Hdlr {
                handler: if video { b"vide".into() } else { b"soun".into() },
                name: if video { "VideoHandler".into() } else { "SoundHandler".into() },
            },
            minf,
        },
        ..Default::default()
    })
}

fn visual(width: u16, height: u16) -> Visual {
    Visual {
        data_reference_index: 1,
        width,
        height,
        horizresolution: 72.into(),
        vertresolution: 72.into(),
        frame_count: 1,
        compressor: "".into(),
        depth: 24,
    }
}

/// One track's run inside a fragment.
pub struct Run<'a> {
    pub track_id: u32,
    pub samples: &'a [Sample],
}

/// Writes one `moof` + `mdat` holding every run, in order. Runs with no
/// samples are skipped.
pub fn fragment(sequence: u32, runs: &[Run<'_>]) -> Bytes {
    let runs: Vec<&Run<'_>> = runs.iter().filter(|r| !r.samples.is_empty()).collect();
    let mut moof = Moof {
        mfhd: Mfhd { sequence_number: sequence },
        traf: runs
            .iter()
            .map(|r| Traf {
                tfhd: Tfhd { track_id: r.track_id, default_base_is_moof: true, ..Default::default() },
                tfdt: Some(Tfdt { base_media_decode_time: r.samples[0].dts.max(0) as u64 }),
                trun: vec![Trun {
                    data_offset: Some(0),
                    entries: r
                        .samples
                        .iter()
                        .map(|s| TrunEntry {
                            duration: Some(s.dur),
                            size: Some(s.data.len() as u32),
                            flags: Some(if s.key { FLAGS_SYNC } else { FLAGS_NON_SYNC }),
                            cts: Some(s.cts),
                        })
                        .collect(),
                }],
                ..Default::default()
            })
            .collect(),
    };
    // The size of the moof does not depend on the offsets (always present),
    // so encode once to measure, then again with the real offsets.
    let mut probe = Vec::new();
    moof.encode(&mut probe).expect("moof encodes");
    let mut offset = probe.len() as i32 + 8;
    for (traf, run) in moof.traf.iter_mut().zip(&runs) {
        traf.trun[0].data_offset = Some(offset);
        offset += run.samples.iter().map(|s| s.data.len() as i32).sum::<i32>();
    }
    let payload: usize = runs.iter().flat_map(|r| r.samples.iter()).map(|s| s.data.len()).sum();
    let mut out = Vec::with_capacity(probe.len() + 8 + payload);
    moof.encode(&mut out).expect("moof encodes");
    debug_assert_eq!(out.len(), probe.len());
    out.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
    out.extend_from_slice(b"mdat");
    for s in runs.iter().flat_map(|r| r.samples.iter()) {
        out.extend_from_slice(&s.data);
    }
    out.into()
}

/// One track out of muxed fragments: for every `moof` + `mdat` pair in
/// `data` (as [`fragment`] writes them, possibly several back to back), a
/// `moof` holding only `track_id`'s `traf` (data offset fixed up) and an
/// `mdat` holding only its samples. Pairs without a run for the track are
/// left out. `None` when `data` is not a sequence of such pairs.
///
/// MPEG-DASH players want one media type per Representation, while the
/// LL-HLS segments carry video and audio together; this turns one stored
/// segment into either view without storing it twice.
pub fn extract_track(data: &[u8], track_id: u32) -> Option<Bytes> {
    let mut out = Vec::with_capacity(data.len());
    let mut at = 0usize;
    let header = |at: usize| -> Option<(usize, [u8; 4])> {
        let h = data.get(at..at + 8)?;
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize;
        (size >= 8 && at + size <= data.len()).then(|| (size, [h[4], h[5], h[6], h[7]]))
    };
    while at < data.len() {
        let (moof_size, kind) = header(at)?;
        if &kind != b"moof" {
            return None;
        }
        let moof_start = at;
        let mut moof = Moof::decode(&mut &data[at..at + moof_size]).ok()?;
        at += moof_size;
        let (mdat_size, kind) = header(at)?;
        if &kind != b"mdat" {
            return None;
        }
        at += mdat_size;
        let Some(mut traf) = moof.traf.drain(..).find(|t| t.tfhd.track_id == track_id) else { continue };
        if traf.trun.len() != 1 {
            return None;
        }
        let run = &traf.trun[0];
        let len: usize = run.entries.iter().map(|e| e.size.map(|s| s as usize)).sum::<Option<usize>>()?;
        let from = moof_start.checked_add_signed(run.data_offset? as isize)?;
        let payload = data.get(from..from.checked_add(len)?)?;
        traf.trun[0].data_offset = Some(0);
        let mut one = Moof { mfhd: moof.mfhd.clone(), traf: vec![traf] };
        let mut probe = Vec::new();
        one.encode(&mut probe).ok()?;
        one.traf[0].trun[0].data_offset = Some(probe.len() as i32 + 8);
        let before = out.len();
        one.encode(&mut out).ok()?;
        debug_assert_eq!(out.len() - before, probe.len());
        out.extend_from_slice(&((len + 8) as u32).to_be_bytes());
        out.extend_from_slice(b"mdat");
        out.extend_from_slice(payload);
    }
    Some(out.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn samples(base: i64, n: usize, byte: u8) -> Vec<Sample> {
        (0..n)
            .map(|i| Sample {
                dts: base + i as i64 * 10,
                cts: 0,
                dur: 10,
                key: i == 0,
                data: Bytes::from(vec![byte; 3 + i]),
            })
            .collect()
    }

    #[test]
    fn extract_track_keeps_one_run_per_fragment() {
        let (v1, a1, v2) = (samples(0, 3, 0x11), samples(5, 2, 0xa1), samples(30, 2, 0x22));
        let mut data = fragment(1, &[Run { track_id: 1, samples: &v1 }, Run { track_id: 2, samples: &a1 }]).to_vec();
        // A second fragment with no audio run at all.
        data.extend_from_slice(&fragment(2, &[Run { track_id: 1, samples: &v2 }, Run { track_id: 2, samples: &[] }]));

        let audio = extract_track(&data, 2).unwrap();
        let mut buf = &audio[..];
        let moof = Moof::decode(&mut buf).unwrap();
        assert_eq!(moof.traf.len(), 1);
        assert_eq!(moof.traf[0].tfhd.track_id, 2);
        assert_eq!(moof.traf[0].tfdt.as_ref().unwrap().base_media_decode_time, 5);
        let mdat = mp4_atom::Mdat::decode(&mut buf).unwrap();
        assert_eq!(mdat.data, [vec![0xa1; 3], vec![0xa1; 4]].concat());
        assert!(buf.is_empty(), "the audio-less fragment is left out");
        let off = moof.traf[0].trun[0].data_offset.unwrap() as usize;
        assert_eq!(&audio[off..off + 3], &[0xa1; 3]);

        let video = extract_track(&data, 1).unwrap();
        let mut buf = &video[..];
        let mut seqs = Vec::new();
        let mut payload = Vec::new();
        while !buf.is_empty() {
            let m = Moof::decode(&mut buf).unwrap();
            assert_eq!(m.traf.len(), 1);
            seqs.push(m.mfhd.sequence_number);
            payload.extend(mp4_atom::Mdat::decode(&mut buf).unwrap().data);
        }
        assert_eq!(seqs, [1, 2]);
        let want: Vec<u8> = v1.iter().chain(&v2).flat_map(|s| s.data.to_vec()).collect();
        assert_eq!(payload, want);

        assert!(extract_track(b"garbage!", 1).is_none());
    }
}
