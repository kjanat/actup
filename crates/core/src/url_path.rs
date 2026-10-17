//! Percent-encoding for the path and query parts of provider URLs.

use std::fmt::Write;

/// Percent-encode `path` for a URL, keeping its `/` separators.
///
/// Every byte except ASCII letters, digits, `-`, `.`, `_`, `~` and `/` becomes
/// `%XX`.
#[must_use]
pub fn encode_path(path: &str) -> String {
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_path_keeps_separators_and_unreserved_bytes() {
        assert_eq!(encode_path("release/v1.0_x~y-z"), "release/v1.0_x~y-z");
        assert_eq!(encode_path("feat#1"), "feat%231");
        assert_eq!(encode_path("v1.0.0+build"), "v1.0.0%2Bbuild");
        assert_eq!(encode_path("a%b?c d"), "a%25b%3Fc%20d");
        assert_eq!(encode_path("é"), "%C3%A9");
    }
}
