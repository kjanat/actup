//! LSP position encoding negotiation and byte↔column conversion.
//!
//! The parser in `actup-core` produces byte offsets (UTF-8 code units) for
//! `uses:` value locations. LSP positions use a negotiated encoding for
//! `Position::character` columns: UTF-16 (the mandatory default) or UTF-8
//! (preferred when the client offers it, since it matches our native offsets).
//!
//! This module negotiates the encoding during `initialize` and provides
//! conversion functions used by diagnostics, hover, and completion handlers.

use tower_lsp_server::ls_types::{ClientCapabilities, PositionEncodingKind};

/// Position encoding negotiated with the LSP client for the session.
///
/// Determines how `Position::character` column offsets are interpreted.
/// Set once during `initialize` and read by all subsequent handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PositionEncoding {
    /// UTF-8 code units (== bytes). Our native format — zero-cost conversion.
    Utf8,
    /// UTF-16 code units. Mandatory fallback per LSP 3.17 spec.
    /// Requires conversion for characters outside the Basic Multilingual Plane.
    Utf16,
}

impl Default for PositionEncoding {
    /// Defaults to UTF-16 per LSP spec (mandatory encoding).
    fn default() -> Self {
        Self::Utf16
    }
}

impl PositionEncoding {
    /// Negotiate encoding from client capabilities.
    ///
    /// Prefers UTF-8 if the client advertises it (avoids conversion since
    /// the parser natively produces byte offsets). Falls back to UTF-16
    /// which is mandatory per the LSP 3.17 spec.
    ///
    /// Per spec: if the client omits `positionEncodings`, the server must
    /// use UTF-16.
    pub fn negotiate(capabilities: &ClientCapabilities) -> Self {
        let offered = capabilities
            .general
            .as_ref()
            .and_then(|g| g.position_encodings.as_ref());

        let Some(encodings) = offered else {
            return Self::Utf16;
        };

        if encodings.contains(&PositionEncodingKind::UTF8) {
            Self::Utf8
        } else {
            Self::Utf16
        }
    }

    /// Convert to the corresponding [`PositionEncodingKind`] for the
    /// `ServerCapabilities` response.
    pub const fn to_lsp(self) -> PositionEncodingKind {
        match self {
            Self::Utf8 => PositionEncodingKind::UTF8,
            Self::Utf16 => PositionEncodingKind::UTF16,
        }
    }
}

/// Convert a byte offset within `line` to an LSP column in the negotiated encoding.
///
/// Used for **outgoing** positions: diagnostic ranges, hover ranges.
///
/// # Panics
///
/// Panics if `byte_offset > line.len()` or falls on a non-UTF-8-char boundary.
pub fn byte_to_col(line: &str, byte_offset: usize, encoding: PositionEncoding) -> u32 {
    let units = match encoding {
        PositionEncoding::Utf8 => byte_offset,
        PositionEncoding::Utf16 => line[..byte_offset].chars().map(char::len_utf16).sum(),
    };
    u32::try_from(units).unwrap_or(u32::MAX)
}

/// Convert an LSP column in the negotiated encoding to a byte offset within `line`.
///
/// Used for **incoming** positions: hover cursor, completion cursor.
///
/// If `col` falls inside a surrogate pair (non-BMP character), advances
/// to the next character boundary (same approach as rust-analyzer).
/// Returns `line.len()` if `col` is past the end.
pub fn col_to_byte(line: &str, col: u32, encoding: PositionEncoding) -> usize {
    let col = usize::try_from(col).unwrap_or(usize::MAX);
    match encoding {
        PositionEncoding::Utf8 => col.min(line.len()),
        PositionEncoding::Utf16 => {
            let mut utf16_count = 0usize;
            for (byte_idx, ch) in line.char_indices() {
                if utf16_count >= col {
                    return byte_idx;
                }
                utf16_count += ch.len_utf16();
            }
            line.len()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    type TestResult = Result<(), std::num::TryFromIntError>;

    #[test]
    fn ascii_offsets_are_identity() -> TestResult {
        let line = "      - uses: actions/checkout@v4";
        for enc in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            for offset in 0..=line.len() {
                assert_eq!(byte_to_col(line, offset, enc), u32::try_from(offset)?);
                assert_eq!(col_to_byte(line, u32::try_from(offset)?, enc), offset);
            }
        }
        Ok(())
    }

    #[test]
    fn utf8_encoding_is_identity() -> TestResult {
        // "données" has é (2 bytes UTF-8, 1 UTF-16 code unit)
        let line = "uses: owner/repo/données/action@v1";
        let byte_offset = line.find("action").unwrap();

        // UTF-8: byte offset is the column
        assert_eq!(
            byte_to_col(line, byte_offset, PositionEncoding::Utf8),
            u32::try_from(byte_offset)?
        );
        assert_eq!(
            col_to_byte(line, u32::try_from(byte_offset)?, PositionEncoding::Utf8),
            byte_offset
        );
        Ok(())
    }

    #[test]
    fn utf16_handles_multibyte_chars() -> TestResult {
        // "é" is 2 bytes UTF-8, 1 UTF-16 code unit
        let line = "uses: owner/repo/données/action@v1";
        let e_acute_byte = line.find('é').unwrap();
        let after_accent_byte = e_acute_byte + 'é'.len_utf8(); // byte after é

        // Before é: UTF-8 == UTF-16
        assert_eq!(
            byte_to_col(line, e_acute_byte, PositionEncoding::Utf16),
            u32::try_from(e_acute_byte)?,
        );

        // After é: UTF-16 col is 1 less than byte offset (2 bytes → 1 code unit)
        assert_eq!(
            byte_to_col(line, after_accent_byte, PositionEncoding::Utf16),
            u32::try_from(after_accent_byte - 1)?,
        );
        Ok(())
    }

    #[test]
    fn utf16_handles_non_bmp_emoji() -> TestResult {
        // 🎉 is 4 bytes UTF-8, 2 UTF-16 code units (surrogate pair)
        let line = "uses: owner/repo@release-🎉-v1";
        let emoji_byte = line.find('🎉').unwrap();
        let after_emoji_byte = emoji_byte + '🎉'.len_utf8(); // +4 bytes

        // Before emoji: all ASCII, offsets match
        assert_eq!(
            byte_to_col(line, emoji_byte, PositionEncoding::Utf16),
            u32::try_from(emoji_byte)?,
        );

        // After emoji: 4 bytes → 2 UTF-16 code units, so col is 2 less
        assert_eq!(
            byte_to_col(line, after_emoji_byte, PositionEncoding::Utf16),
            u32::try_from(after_emoji_byte - 2)?,
        );
        Ok(())
    }

    #[test]
    fn col_to_byte_roundtrips_with_byte_to_col() {
        let line = "uses: owner/repo/données/action@release-🎉-v1";

        for enc in [PositionEncoding::Utf8, PositionEncoding::Utf16] {
            // For every valid byte boundary, roundtrip should hold
            for (byte_offset, _) in line.char_indices() {
                let col = byte_to_col(line, byte_offset, enc);
                let back = col_to_byte(line, col, enc);
                assert_eq!(
                    back, byte_offset,
                    "roundtrip failed at byte {byte_offset} with {enc:?}"
                );
            }
            // Also test end of string
            let col = byte_to_col(line, line.len(), enc);
            let back = col_to_byte(line, col, enc);
            assert_eq!(back, line.len());
        }
    }

    #[test]
    fn col_to_byte_clamps_inside_surrogate_pair() {
        // 🎉 = 2 UTF-16 code units. If col points to the second unit,
        // we should clamp to the start of the character.
        let line = "A🎉B";
        // A=1 UTF-16 cu, 🎉=2 UTF-16 cu, B=1 UTF-16 cu
        // col 0 → byte 0 (A)
        // col 1 → byte 1 (start of 🎉)
        // col 2 → byte 5 (inside surrogate pair, advances to next boundary)
        // col 3 → byte 5 (B)
        assert_eq!(col_to_byte(line, 0, PositionEncoding::Utf16), 0);
        assert_eq!(col_to_byte(line, 1, PositionEncoding::Utf16), 1);
        // col 2 is the second code unit of the surrogate pair — 🎉 contributes
        // 2 code units, so after processing 🎉 utf16_count=3 which is > 2,
        // meaning the loop exits at byte 1+4=5 check. Actually let me trace:
        // char_indices: (0,'A'), (1,'🎉'), (5,'B')
        // At (0,'A'): utf16_count=0 < 2, add 1 → utf16_count=1
        // At (1,'🎉'): utf16_count=1 < 2, add 2 → utf16_count=3
        // At (5,'B'): utf16_count=3 >= 2, return 5
        // So col 2 → byte 5 (B). This is correct per LSP spec: col 2 is
        // the second unit of the surrogate pair, which resolves to the next
        // character boundary.
        assert_eq!(col_to_byte(line, 2, PositionEncoding::Utf16), 5);
        assert_eq!(col_to_byte(line, 3, PositionEncoding::Utf16), 5);
    }

    #[test]
    fn col_to_byte_past_end_returns_line_len() {
        let line = "hello";
        assert_eq!(col_to_byte(line, 100, PositionEncoding::Utf8), line.len());
        assert_eq!(col_to_byte(line, 100, PositionEncoding::Utf16), line.len());
    }

    #[test]
    fn negotiate_prefers_utf8() {
        use tower_lsp_server::ls_types::GeneralClientCapabilities;

        let caps = ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![
                    PositionEncodingKind::UTF16,
                    PositionEncodingKind::UTF8,
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(PositionEncoding::negotiate(&caps), PositionEncoding::Utf8);
    }

    #[test]
    fn negotiate_falls_back_to_utf16_when_utf8_absent() {
        use tower_lsp_server::ls_types::GeneralClientCapabilities;

        let caps = ClientCapabilities {
            general: Some(GeneralClientCapabilities {
                position_encodings: Some(vec![PositionEncodingKind::UTF16]),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(PositionEncoding::negotiate(&caps), PositionEncoding::Utf16);
    }

    #[test]
    fn negotiate_falls_back_to_utf16_when_none() {
        let caps = ClientCapabilities::default();
        assert_eq!(PositionEncoding::negotiate(&caps), PositionEncoding::Utf16);
    }

    #[test]
    fn byte_to_col_empty_string_returns_zero() {
        assert_eq!(byte_to_col("", 0, PositionEncoding::Utf8), 0);
        assert_eq!(byte_to_col("", 0, PositionEncoding::Utf16), 0);
    }

    #[test]
    fn col_to_byte_empty_string_returns_zero() {
        assert_eq!(col_to_byte("", 0, PositionEncoding::Utf8), 0);
        assert_eq!(col_to_byte("", 100, PositionEncoding::Utf8), 0);
    }

    #[test]
    fn position_encoding_default_is_utf16() {
        assert_eq!(PositionEncoding::default(), PositionEncoding::Utf16);
    }

    #[test]
    fn to_lsp_conversion() {
        assert_eq!(PositionEncoding::Utf8.to_lsp(), PositionEncodingKind::UTF8);
        assert_eq!(
            PositionEncoding::Utf16.to_lsp(),
            PositionEncodingKind::UTF16
        );
    }

    #[test]
    fn utf16_handles_multiple_non_bmp_chars() {
        let line = "🎉🚀✨";
        let after_first = '🎉'.len_utf8();
        let after_second = after_first + '🚀'.len_utf8();

        assert_eq!(
            byte_to_col(line, after_first, PositionEncoding::Utf16),
            2 // 🎉 = 2 UTF-16 code units (surrogate pair)
        );
        assert_eq!(
            byte_to_col(line, after_second, PositionEncoding::Utf16),
            4 // 🎉(2) + 🚀(2) = 4 UTF-16 code units
        );
    }
}
