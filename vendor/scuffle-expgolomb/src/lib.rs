//! A set of helper functions to encode and decode exponential-golomb values.
//!
//! This crate extends upon the [`BitReader`] and [`BitWriter`] from the
//! [`scuffle-bytes-util`][scuffle_bytes_util] crate to provide functionality
//! for reading and writing Exp-Golomb encoded numbers.
#![cfg_attr(feature = "docs", doc = "\n\nSee the [changelog][changelog] for a full release history.")]
#![cfg_attr(feature = "docs", doc = "## Feature flags")]
#![cfg_attr(feature = "docs", doc = document_features::document_features!())]
//! ## Usage
//!
//! ```rust
//! # fn test() -> std::io::Result<()> {
//! use scuffle_expgolomb::{BitReaderExpGolombExt, BitWriterExpGolombExt};
//! use scuffle_bytes_util::{BitReader, BitWriter};
//!
//! let mut bit_writer = BitWriter::default();
//! bit_writer.write_exp_golomb(0)?;
//! bit_writer.write_exp_golomb(1)?;
//! bit_writer.write_exp_golomb(2)?;
//!
//! let data: Vec<u8> = bit_writer.finish()?;
//!
//! let mut bit_reader = BitReader::new(std::io::Cursor::new(data));
//!
//! let result = bit_reader.read_exp_golomb()?;
//! assert_eq!(result, 0);
//!
//! let result = bit_reader.read_exp_golomb()?;
//! assert_eq!(result, 1);
//!
//! let result = bit_reader.read_exp_golomb()?;
//! assert_eq!(result, 2);
//! # Ok(())
//! # }
//! # test().expect("failed to run test");
//! ```
//!
//! ## License
//!
//! This project is licensed under the MIT or Apache-2.0 license.
//! You can choose between one of them if you use this work.
//!
//! `SPDX-License-Identifier: MIT OR Apache-2.0`
#![cfg_attr(all(coverage_nightly, test), feature(coverage_attribute))]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![deny(missing_docs)]
#![deny(unsafe_code)]
#![deny(unreachable_pub)]

use std::io;

use scuffle_bytes_util::{BitReader, BitWriter};

/// Extension trait for reading Exp-Golomb encoded numbers from a bit reader
///
/// See: <https://en.wikipedia.org/wiki/Exponential-Golomb_coding>
///
/// - [`BitReader`]
pub trait BitReaderExpGolombExt {
    /// Reads an Exp-Golomb encoded number
    fn read_exp_golomb(&mut self) -> io::Result<u64>;

    /// Reads a signed Exp-Golomb encoded number
    fn read_signed_exp_golomb(&mut self) -> io::Result<i64> {
        let exp_glob = self.read_exp_golomb()?;
        // Caudal patch: `exp_glob` can be as large as `u64::MAX - 1` (see
        // `read_exp_golomb`'s own patch above), so `exp_glob / 2` can
        // exceed `i64::MAX`. The plain `as i64` cast below doesn't panic,
        // but reinterprets the bits into an unrelated negative number, and
        // the following `-`/`+ 1` on THAT could then overflow and panic
        // (found via `scuffle-h264`'s SPS extension scaling-matrix parsing
        // by `fuzz/fuzz_targets/rtmp_flv_amf.rs`). Saturating the u64 ->
        // i64 conversion first keeps every value a real encoder would ever
        // produce (which fits comfortably in an i64) byte-for-byte
        // identical, and turns the unrepresentable range into `i64::MIN`/
        // `i64::MAX` instead of a panic or a nonsense sign flip.
        let half = i64::try_from(exp_glob / 2).unwrap_or(i64::MAX);

        if exp_glob.is_multiple_of(2) {
            Ok(half.saturating_neg())
        } else {
            Ok(half.saturating_add(1))
        }
    }
}

impl<R: io::Read> BitReaderExpGolombExt for BitReader<R> {
    fn read_exp_golomb(&mut self) -> io::Result<u64> {
        let mut leading_zeros = 0;
        while !self.read_bit()? {
            leading_zeros += 1;
            // Caudal patch: 63 leading zero bits is the largest codeNum a
            // u64 can hold (`result` maxes out at exactly `u64::MAX`, so
            // `result - 1` is still in range: see
            // `test_exp_glob_encode`/`test_signed_exp_glob_encode`, which
            // round-trip `u64::MAX - 1`/`i64::MAX` through exactly that
            // many leading zeros). At 64, `result` (built as `1 <<
            // leading_zeros`) has already shifted its one bit out of a
            // u64 to 0, and the final `result - 1` underflowed and
            // panicked. No real H.264/H.265 field legitimately needs more
            // than a u64 can hold; a bitstream that claims to is corrupt
            // or adversarial either way. Found via `scuffle-h264`'s SPS
            // parsing by `fuzz/fuzz_targets/rtmp_flv_amf.rs`.
            if leading_zeros >= 64 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "exp-golomb code too long"));
            }
        }

        let mut result: u64 = 1;
        for _ in 0..leading_zeros {
            result <<= 1;
            result |= self.read_bit()? as u64;
        }

        Ok(result - 1)
    }
}

/// Extension trait for writing Exp-Golomb encoded numbers to a bit writer
///
/// See: <https://en.wikipedia.org/wiki/Exponential-Golomb_coding>
///
/// - [`BitWriter`]
pub trait BitWriterExpGolombExt {
    /// Writes an Exp-Golomb encoded number
    fn write_exp_golomb(&mut self, input: u64) -> io::Result<()>;

    /// Writes a signed Exp-Golomb encoded number
    fn write_signed_exp_golomb(&mut self, number: i64) -> io::Result<()> {
        let number = if number <= 0 {
            -number as u64 * 2
        } else {
            number as u64 * 2 - 1
        };

        self.write_exp_golomb(number)
    }
}

impl<W: io::Write> BitWriterExpGolombExt for BitWriter<W> {
    fn write_exp_golomb(&mut self, input: u64) -> io::Result<()> {
        let mut number = input + 1;
        let mut leading_zeros = 0;
        while number > 1 {
            number >>= 1;
            leading_zeros += 1;
        }

        for _ in 0..leading_zeros {
            self.write_bit(false)?;
        }

        self.write_bits(input + 1, leading_zeros + 1)?;

        Ok(())
    }
}

/// Returns the number of bits that a signed Exp-Golomb encoded number would take up.
///
/// See: <https://en.wikipedia.org/wiki/Exponential-Golomb_coding>
pub fn size_of_signed_exp_golomb(number: i64) -> u64 {
    let number = if number <= 0 {
        -number as u64 * 2
    } else {
        number as u64 * 2 - 1
    };

    size_of_exp_golomb(number)
}

/// Returns the number of bits that an Exp-Golomb encoded number would take up.
///
/// See: <https://en.wikipedia.org/wiki/Exponential-Golomb_coding>
pub fn size_of_exp_golomb(number: u64) -> u64 {
    let mut number = number + 1;
    let mut leading_zeros = 0;
    while number > 1 {
        number >>= 1;
        leading_zeros += 1;
    }

    leading_zeros * 2 + 1
}

#[cfg(test)]
#[cfg_attr(all(test, coverage_nightly), coverage(off))]
mod tests {
    use bytes::Buf;
    use scuffle_bytes_util::{BitReader, BitWriter};

    use crate::{BitReaderExpGolombExt, BitWriterExpGolombExt, size_of_exp_golomb, size_of_signed_exp_golomb};

    pub(crate) fn get_remaining_bits(reader: &BitReader<std::io::Cursor<Vec<u8>>>) -> usize {
        let remaining = reader.get_ref().remaining();

        if reader.is_aligned() {
            remaining * 8
        } else {
            remaining * 8 + (8 - reader.bit_pos() as usize)
        }
    }

    #[test]
    fn test_exp_glob_decode() {
        let mut bit_writer = BitWriter::<Vec<u8>>::default();

        bit_writer.write_bits(0b1, 1).unwrap(); // 0
        bit_writer.write_bits(0b010, 3).unwrap(); // 1
        bit_writer.write_bits(0b011, 3).unwrap(); // 2
        bit_writer.write_bits(0b00100, 5).unwrap(); // 3
        bit_writer.write_bits(0b00101, 5).unwrap(); // 4
        bit_writer.write_bits(0b00110, 5).unwrap(); // 5
        bit_writer.write_bits(0b00111, 5).unwrap(); // 6

        let data = bit_writer.finish().unwrap();

        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));

        let remaining_bits = get_remaining_bits(&bit_reader);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 0);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 1);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 4);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 2);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 7);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 3);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 12);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 4);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 17);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 5);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 22);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 6);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 27);
    }

    /// Caudal patch regression: found via `scuffle-h264`'s SPS parsing (any
    /// exp-Golomb field: width, height, crop offsets, ...) by
    /// `fuzz/fuzz_targets/rtmp_flv_amf.rs`. 64 or more leading zero bits
    /// before the terminating `1` made `result` (built as `1 <<
    /// leading_zeros`) wrap to 0, and the final `result - 1` then
    /// underflowed and panicked -- on a bitstream that doesn't need to be
    /// especially large, just 64+ zero bits in a row.
    #[test]
    fn a_codenum_of_64_or_more_leading_zeros_errors_instead_of_panicking() {
        let mut bit_writer = BitWriter::<Vec<u8>>::default();
        // 64 zero bits, then enough one-bits to satisfy the (never
        // reached, since this must error first) shift loop.
        bit_writer.write_bits(0, 64).unwrap();
        bit_writer.write_bits(u64::MAX, 64).unwrap();
        let data = bit_writer.finish().unwrap();

        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));
        assert!(bit_reader.read_exp_golomb().is_err(), "must error, not panic, on this input");

        // One bit short of that (63 leading zeros) still must not panic
        // either, whether or not it's accepted.
        let mut bit_writer = BitWriter::<Vec<u8>>::default();
        bit_writer.write_bits(0, 63).unwrap();
        bit_writer.write_bits(u64::MAX, 64).unwrap();
        let data = bit_writer.finish().unwrap();
        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));
        let _ = bit_reader.read_exp_golomb(); // Ok or Err, just not a panic.
    }

    #[test]
    fn test_signed_exp_glob_decode() {
        let mut bit_writer = BitWriter::<Vec<u8>>::default();

        bit_writer.write_bits(0b1, 1).unwrap(); // 0
        bit_writer.write_bits(0b010, 3).unwrap(); // 1
        bit_writer.write_bits(0b011, 3).unwrap(); // -1
        bit_writer.write_bits(0b00100, 5).unwrap(); // 2
        bit_writer.write_bits(0b00101, 5).unwrap(); // -2
        bit_writer.write_bits(0b00110, 5).unwrap(); // 3
        bit_writer.write_bits(0b00111, 5).unwrap(); // -3

        let data = bit_writer.finish().unwrap();

        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));

        let remaining_bits = get_remaining_bits(&bit_reader);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 0);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 1);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 4);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, -1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 7);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 2);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 12);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, -2);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 17);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 3);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 22);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, -3);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 27);
    }

    /// The largest codeNum `read_exp_golomb` can return (`u64::MAX - 1`,
    /// capped by its own patch above) is exactly `2 * i64::MAX`, so its
    /// halved value is exactly `i64::MAX`: `read_signed_exp_golomb`'s
    /// `as i64` cast and the negation/`+ 1` that follows it were never
    /// actually reachable in an overflowing state once that cap exists
    /// (this crate's `read_exp_golomb` fix alone was enough to also make
    /// this function safe). The `saturating_neg`/`saturating_add` here are
    /// defense in depth in case that cap ever changes; this test pins the
    /// current, correct boundary behaviour rather than a saturated
    /// sentinel, since nothing actually saturates today.
    #[test]
    fn read_signed_exp_golomb_handles_its_largest_possible_input() {
        // codeNum = u64::MAX - 1 (63 leading zero bits, all-ones suffix):
        // the largest value `read_exp_golomb` can return. Even, so this
        // takes the negation branch, where `exp_glob / 2` is exactly
        // `i64::MAX`.
        let mut bit_writer = BitWriter::<Vec<u8>>::default();
        bit_writer.write_bits(0, 63).unwrap();
        bit_writer.write_bits(u64::MAX, 64).unwrap();
        let data = bit_writer.finish().unwrap();
        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));
        assert_eq!(bit_reader.read_signed_exp_golomb().unwrap(), -i64::MAX, "must not panic");
    }

    #[test]
    fn test_exp_glob_encode() {
        let mut bit_writer = BitWriter::<Vec<u8>>::default();

        bit_writer.write_exp_golomb(0).unwrap();
        bit_writer.write_exp_golomb(1).unwrap();
        bit_writer.write_exp_golomb(2).unwrap();
        bit_writer.write_exp_golomb(3).unwrap();
        bit_writer.write_exp_golomb(4).unwrap();
        bit_writer.write_exp_golomb(5).unwrap();
        bit_writer.write_exp_golomb(6).unwrap();
        bit_writer.write_exp_golomb(u64::MAX - 1).unwrap();

        let data = bit_writer.finish().unwrap();

        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));

        let remaining_bits = get_remaining_bits(&bit_reader);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 0);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 1);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 4);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 2);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 7);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 3);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 12);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 4);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 17);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 5);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 22);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, 6);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 27);

        let result = bit_reader.read_exp_golomb().unwrap();
        assert_eq!(result, u64::MAX - 1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 154);
    }

    #[test]
    fn test_signed_exp_glob_encode() {
        let mut bit_writer = BitWriter::<Vec<u8>>::default();

        bit_writer.write_signed_exp_golomb(0).unwrap();
        bit_writer.write_signed_exp_golomb(1).unwrap();
        bit_writer.write_signed_exp_golomb(-1).unwrap();
        bit_writer.write_signed_exp_golomb(2).unwrap();
        bit_writer.write_signed_exp_golomb(-2).unwrap();
        bit_writer.write_signed_exp_golomb(3).unwrap();
        bit_writer.write_signed_exp_golomb(-3).unwrap();
        bit_writer.write_signed_exp_golomb(i64::MAX).unwrap();

        let data = bit_writer.finish().unwrap();

        let mut bit_reader = BitReader::new(std::io::Cursor::new(data));

        let remaining_bits = get_remaining_bits(&bit_reader);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 0);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 1);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 4);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, -1);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 7);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 2);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 12);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, -2);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 17);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, 3);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 22);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, -3);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 27);

        let result = bit_reader.read_signed_exp_golomb().unwrap();
        assert_eq!(result, i64::MAX);
        assert_eq!(get_remaining_bits(&bit_reader), remaining_bits - 154);
    }

    #[test]
    fn test_expg_sizes() {
        assert_eq!(1, size_of_exp_golomb(0)); // 0b1
        assert_eq!(3, size_of_exp_golomb(1)); // 0b010
        assert_eq!(3, size_of_exp_golomb(2)); // 0b011
        assert_eq!(5, size_of_exp_golomb(3)); // 0b00100
        assert_eq!(5, size_of_exp_golomb(4)); // 0b00101
        assert_eq!(5, size_of_exp_golomb(5)); // 0b00110
        assert_eq!(5, size_of_exp_golomb(6)); // 0b00111

        assert_eq!(1, size_of_signed_exp_golomb(0)); // 0b1
        assert_eq!(3, size_of_signed_exp_golomb(1)); // 0b010
        assert_eq!(3, size_of_signed_exp_golomb(-1)); // 0b011
        assert_eq!(5, size_of_signed_exp_golomb(2)); // 0b00100
        assert_eq!(5, size_of_signed_exp_golomb(-2)); // 0b00101
        assert_eq!(5, size_of_signed_exp_golomb(3)); // 0b00110
        assert_eq!(5, size_of_signed_exp_golomb(-3)); // 0b00111
    }
}

/// Changelogs generated by [scuffle_changelog]
#[cfg(feature = "docs")]
#[scuffle_changelog::changelog]
pub mod changelog {}
