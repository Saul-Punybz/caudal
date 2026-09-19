//! The seam between whatever produces live text for a stream (today
//! `caudal-captions`, local speech-to-text) and the outputs that carry it
//! (today the LL-HLS WebVTT rendition). Outputs depend on this trait, not on
//! the heavy producer.

/// One caption cue on a stream's media timeline: the same microsecond clock
/// as [`crate::Cue::at_us`] (frame timestamps through
/// [`crate::TrackInfo::to_micros`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextCue {
    pub start_us: i64,
    pub end_us: i64,
    /// One or two lines, `\n`-separated. Plain text, never markup.
    pub text: String,
}

/// How a captioned stream's text track is announced to players.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextTrack {
    /// BCP 47 language (`es`, `en`), or `None` when detected per chunk.
    pub language: Option<String>,
    /// Human-readable name for the player's menu.
    pub name: String,
}

pub trait CaptionSource: Send + Sync + 'static {
    /// `Some` when `stream` is configured for captions (from its first
    /// publish, whether or not any text has been produced yet).
    fn track(&self, stream: &str) -> Option<TextTrack>;

    /// Cues overlapping `[from_us, to_us)`, oldest first.
    fn cues(&self, stream: &str, from_us: i64, to_us: i64) -> Vec<TextCue>;
}
