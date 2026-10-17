//! Percent-encoding for the path and query parts of provider URLs.

use std::fmt::Write;

use crate::{Error, Result};

/// Percent-encode `path` for a URL, keeping its `/` separators.
///
/// Every byte except ASCII letters, digits, `-`, `.`, `_`, `~` and `/` becomes
/// `%XX`.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when a segment of `path` is `.` or `..`.
/// URL parsers resolve those segments, so the request would leave the
/// repository.
pub fn encode_path(path: &str) -> Result<String> {
    if path.split('/').any(|segment| matches!(segment, "." | "..")) {
        return Err(Error::InvalidRef {
            input: path.to_string(),
            reason: "a `.` or `..` path segment".to_string(),
        });
    }
    Ok(encode(path, b"/"))
}

/// Percent-encode `value` as one URL path segment or query value, `/` included.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `value` is empty, `.` or `..`.
pub fn encode_segment(value: &str) -> Result<String> {
    if matches!(value, "" | "." | "..") {
        return Err(Error::InvalidRef {
            input: value.to_string(),
            reason: "an empty, `.` or `..` path segment".to_string(),
        });
    }
    Ok(encode(value, b""))
}

fn encode(value: &str, kept: &[u8]) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'.' | b'_' | b'~')
            || kept.contains(&byte)
        {
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
    fn encode_path_keeps_separators_and_unreserved_bytes() -> Result<()> {
        assert_eq!(encode_path("release/v1.0_x~y-z")?, "release/v1.0_x~y-z");
        assert_eq!(encode_path("feat#1")?, "feat%231");
        assert_eq!(encode_path("v1.0.0+build")?, "v1.0.0%2Bbuild");
        assert_eq!(encode_path("a%b?c d")?, "a%25b%3Fc%20d");
        assert_eq!(encode_path("%2e%2e")?, "%252e%252e");
        assert_eq!(encode_path("é")?, "%C3%A9");
        assert_eq!(encode_path(".github/x..y")?, ".github/x..y");
        Ok(())
    }

    #[test]
    fn encode_segment_encodes_slashes_and_refuses_dot_values() -> Result<()> {
        assert_eq!(encode_segment("o/r")?, "o%2Fr");
        assert_eq!(encode_segment("release/v1+b")?, "release%2Fv1%2Bb");
        assert_eq!(encode_segment("../x")?, "..%2Fx");
        for value in ["", ".", ".."] {
            assert!(
                matches!(encode_segment(value), Err(Error::InvalidRef { .. })),
                "{value:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn encode_path_refuses_dot_segments() {
        for path in ["..", ".", "../../x/y", "a/./b", "a/.."] {
            assert!(
                matches!(encode_path(path), Err(Error::InvalidRef { .. })),
                "{path}"
            );
        }
    }
}
