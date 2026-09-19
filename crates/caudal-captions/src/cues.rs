//! Recognised text → caption cues on the stream's timeline, and the
//! per-stream store the outputs read them from.
//!
//! Timing: a chunk's text only exists a few seconds after its speech (the
//! chunk has to end, then be transcribed). Its cues start at the stream's
//! live edge at the moment the text is ready, not back at the speech: an
//! output has already sent what came before the edge, and a cue placed
//! there would never be seen. Viewers therefore read the words a few
//! seconds after they hear them, like a live stenographer's captions.

use std::collections::VecDeque;

use caudal_core::captions::TextCue;

/// Characters per caption line (the common broadcast limit).
pub const LINE: usize = 42;
/// Lines per cue.
const LINES: usize = 2;
/// Media time kept in the store.
const KEEP_US: i64 = 120_000_000;

/// Word-wraps `text` into lines of at most [`LINE`] characters (a longer
/// word gets a line of its own).
pub fn wrap(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for w in text.split_whitespace() {
        if !cur.is_empty() && cur.chars().count() + 1 + w.chars().count() > LINE {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(w);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// One chunk's cues, and where the next chunk's may start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Laid {
    pub cues: Vec<TextCue>,
    /// The end of the last cue's share of the speech (its reading time,
    /// without the `min_display_us` hold). The next chunk's cues start no
    /// earlier, or they would cover this text before it was read.
    pub next_free_us: i64,
}

/// Cues for one chunk's `text`, starting at `start_us`. The chunk's
/// speech lasted `speech_us`; it is shared between the cues by length
/// (at least 1 s each). The last cue stays up at least `min_display_us`
/// (so a player that fetches the segment a little late still shows it);
/// earlier ones end where the next begins.
pub fn layout(text: &str, start_us: i64, speech_us: i64, min_display_us: i64) -> Laid {
    let lines = wrap(text);
    let groups: Vec<String> = lines.chunks(LINES).map(|g| g.join("\n")).collect();
    let total: usize = groups.iter().map(|g| g.chars().count()).sum::<usize>().max(1);
    let speech_us = speech_us.max(1_000_000);
    let mut t = start_us;
    let mut cues = Vec::with_capacity(groups.len());
    let n = groups.len();
    for (i, g) in groups.into_iter().enumerate() {
        let share = (speech_us as i128 * g.chars().count() as i128 / total as i128) as i64;
        let share = share.max(1_000_000);
        let end = if i + 1 == n { t + share.max(min_display_us) } else { t + share };
        cues.push(TextCue { start_us: t, end_us: end, text: g });
        t += share;
    }
    Laid { cues, next_free_us: t }
}

/// Cues of one publish of one stream (a republish gets a new store: its
/// clock starts over), oldest first.
#[derive(Default)]
pub struct CueStore {
    cues: VecDeque<TextCue>,
    next_free: Option<i64>,
}

impl CueStore {
    /// Where the next chunk's cues may start: after the reading time of
    /// the last ones.
    pub fn next_free(&self) -> Option<i64> {
        self.next_free
    }

    /// Adds one chunk's cues.
    pub fn add(&mut self, laid: Laid) {
        if !laid.cues.is_empty() {
            self.next_free = Some(self.next_free.map_or(laid.next_free_us, |n| n.max(laid.next_free_us)));
        }
        for c in laid.cues {
            let at = self.cues.iter().rposition(|q| q.start_us <= c.start_us).map_or(0, |i| i + 1);
            self.cues.insert(at, c);
        }
        if let Some(newest) = self.cues.back().map(|c| c.start_us) {
            while self.cues.front().is_some_and(|c| c.end_us < newest - KEEP_US) {
                self.cues.pop_front();
            }
        }
    }

    pub fn overlapping(&self, from_us: i64, to_us: i64) -> Vec<TextCue> {
        self.cues.iter().filter(|c| c.start_us < to_us && c.end_us > from_us).cloned().collect()
    }

    pub fn len(&self) -> usize {
        self.cues.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cues.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_at_word_boundaries_within_the_line_limit() {
        let text = "Les recomendamos preparar agua, comida y baterías para varios días en caso de emergencia.";
        let lines = wrap(text);
        assert!(lines.iter().all(|l| l.chars().count() <= LINE), "{lines:?}");
        assert_eq!(lines.join(" "), text);
        assert_eq!(wrap("  "), Vec::<String>::new());
        let long = "x".repeat(50);
        assert_eq!(wrap(&format!("a {long} b")), vec!["a".to_owned(), long, "b".to_owned()]);
    }

    #[test]
    fn short_text_is_one_cue_held_for_the_minimum() {
        let laid = layout("Hola a todos.", 10_000_000, 1_500_000, 3_000_000);
        assert_eq!(laid.cues, vec![TextCue { start_us: 10_000_000, end_us: 13_000_000, text: "Hola a todos.".into() }]);
        assert_eq!(laid.next_free_us, 11_500_000, "the next chunk waits for the reading time, not the hold");
    }

    #[test]
    fn long_text_is_split_into_back_to_back_cues_of_two_lines() {
        let text = "Buenas tardes y bienvenidos a la transmisión en vivo. Hoy vamos a hablar del clima en Puerto Rico durante la temporada de huracanes.";
        let cues = layout(text, 0, 8_000_000, 3_000_000).cues;
        assert_eq!(cues.len(), 2);
        assert!(cues.iter().all(|c| c.text.lines().count() <= 2));
        assert_eq!(cues[0].end_us, cues[1].start_us, "no gap, no overlap inside a chunk");
        // Time is shared by length: the first cue has most of the text.
        assert!(cues[0].end_us > 4_000_000 && cues[0].end_us < 8_000_000, "{cues:?}");
        assert!(cues[1].end_us - cues[1].start_us >= 3_000_000);
        let joined: Vec<String> = cues.iter().map(|c| c.text.replace('\n', " ")).collect();
        assert_eq!(joined.join(" "), text);
    }

    #[test]
    fn store_answers_overlaps_and_forgets_old_cues() {
        let mut s = CueStore::default();
        s.add(layout("uno", 0, 1_000_000, 1_000_000));
        s.add(layout("dos", 5_000_000, 1_000_000, 1_000_000));
        assert_eq!(s.overlapping(0, 2_000_000).len(), 1);
        assert_eq!(s.overlapping(500_000, 5_500_000).len(), 2);
        assert!(s.overlapping(1_000_000, 5_000_000).is_empty(), "end and start are exclusive");
        // Out of order (a chunk laid out ahead of the next one's edge):
        // kept, in start order.
        s.add(layout("entre", 4_000_000, 1_000_000, 1_000_000));
        let starts: Vec<i64> = s.overlapping(0, i64::MAX).iter().map(|c| c.start_us).collect();
        assert_eq!(starts, vec![0, 4_000_000, 5_000_000]);
        s.add(layout("tres", 200_000_000, 1_000_000, 1_000_000));
        assert_eq!(s.len(), 1, "cues more than two minutes old are dropped");
    }

    #[test]
    fn the_store_remembers_where_the_next_chunk_may_start() {
        let mut s = CueStore::default();
        assert_eq!(s.next_free(), None);
        s.add(layout("uno dos tres", 0, 4_000_000, 6_000_000));
        assert_eq!(s.next_free(), Some(4_000_000));
        s.add(layout("", 9_000_000, 1_000_000, 1_000_000));
        assert_eq!(s.next_free(), Some(4_000_000), "nothing said moves nothing");
    }
}
