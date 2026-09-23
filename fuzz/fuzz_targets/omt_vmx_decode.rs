//! Fuzzes `vmx_codec::Decoder` the way Caudal's OMT pull uses it: every
//! video frame an OMT sender puts on the LAN is decoded here, with the frame
//! size taken from the (equally untrusted) OMT frame header. Nothing may
//! panic, hang or allocate without bound; errors are fine.
//!
//! The OMT repository fuzzes the codec on its own (`vmx_decode` there, rev
//! 8c275ff); this target is Caudal's copy of the rule "every parser gets a
//! fuzz target", run in Caudal's fuzz-smoke CI against the exact rev Caudal
//! pins, and it follows Caudal's call pattern: one `Decoder` reused across
//! many frames of one size (a pull decodes a whole session with one), the
//! header read (`info`) before the decode, and output to UYVY (8-bit) or
//! P216 (10-bit), plus the 1/8 preview.
//!
//! Input: `w w h h sel` then frames, each `len_lo len_hi` + that many bytes
//! (the last frame takes whatever is left). Width and height are modulo
//! 2049 (invalid sizes included) so one input cannot ask for gigabytes.
//! `sel` bit 0: 10-bit output; bit 1: alpha; bits 2-3: extra decode threads.

#![no_main]

use libfuzzer_sys::fuzz_target;
use vmx_codec::{Decoder, PixelFormat};

fuzz_target!(|data: &[u8]| {
    if data.len() < 5 {
        return;
    }
    let width = usize::from(u16::from_le_bytes([data[0], data[1]])) % 2049;
    let height = usize::from(u16::from_le_bytes([data[2], data[3]])) % 2049;
    let sel = data[4];
    let Ok(mut dec) = Decoder::new(width, height) else {
        return;
    };
    dec.set_threads(1 + usize::from((sel >> 2) & 3));
    let format = match (sel & 1 != 0, sel & 2 != 0) {
        (false, false) => PixelFormat::Uyvy,
        (false, true) => PixelFormat::Uyva,
        (true, false) => PixelFormat::P216,
        (true, true) => PixelFormat::Pa16,
    };
    let mut rest = &data[5..];
    while !rest.is_empty() {
        let frame = if rest.len() >= 2 {
            let n = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
            let body = &rest[2..];
            let n = n.min(body.len());
            let (f, r) = body.split_at(n);
            rest = r;
            f
        } else {
            std::mem::take(&mut rest)
        };
        // A bad header must fail both ways, never panic either way.
        let _ = dec.info(frame);
        if let Ok(f) = dec.decode(frame, format) {
            assert_eq!((f.width, f.height), (width, height));
        }
        if let Ok(n) = dec.preview_len(frame) {
            assert!(n <= frame.len(), "preview_len {n} > {}", frame.len());
            let _ = dec.decode_preview(&frame[..n], sel & 2 != 0);
        }
    }
});
