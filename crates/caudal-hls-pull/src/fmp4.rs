//! fMP4 / CMAF segments into a `Publisher`: the init segment (`EXT-X-MAP`)
//! announces the tracks at once, each media segment's frames are pushed
//! in decode order. The timeline is kept monotonic across
//! `EXT-X-DISCONTINUITY` and init changes with the same [`Rebase`] the TS
//! path uses.

use bytes::Bytes;
use caudal_cmaf::demux::Fmp4Demux;
use caudal_core::{Publisher, TrackInfo};
use caudal_ts::ingest::Rebase;
use reqwest::Url;

pub(crate) struct Fmp4Ingest {
    publisher: Publisher,
    demux: Fmp4Demux,
    /// The `EXT-X-MAP` URI `demux` was built from.
    map: Url,
    tracks: Vec<TrackInfo>,
    rebase: Rebase,
}

impl Fmp4Ingest {
    pub(crate) fn new(publisher: Publisher, map: Url, demux: Fmp4Demux) -> Self {
        let mut me = Self { publisher, demux, map, tracks: Vec::new(), rebase: Rebase::default() };
        me.announce();
        me
    }

    fn announce(&mut self) {
        let tracks = self.demux.tracks();
        for t in &tracks {
            self.rebase.timescale(t);
        }
        if tracks != self.tracks {
            if let Err(err) = self.publisher.set_tracks(tracks.clone()) {
                tracing::warn!(stream = %self.publisher.stream().name(), %err, "set_tracks failed");
            }
            self.tracks = tracks;
        }
    }

    pub(crate) fn map(&self) -> &Url {
        &self.map
    }

    /// A new init segment: new tracks (re-announced if they changed) and a
    /// splice, since its timestamps need not follow the old ones.
    pub(crate) fn new_init(&mut self, map: Url, demux: Fmp4Demux) {
        self.demux = demux;
        self.map = map;
        self.announce();
        self.rebase.splice();
    }

    pub(crate) fn discontinuity(&mut self) {
        self.rebase.splice();
    }

    /// Pushes one media segment's frames; returns how many.
    pub(crate) fn feed(&mut self, segment: &Bytes) -> Result<usize, String> {
        let frames = self.demux.segment(segment)?;
        let n = frames.len();
        for mut f in frames {
            self.rebase.apply(&mut f);
            if let Err(err) = self.publisher.push(f) {
                tracing::trace!(%err, "dropped frame");
            }
        }
        Ok(n)
    }
}
