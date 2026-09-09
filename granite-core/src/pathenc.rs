//! `escaped-bytes-v1`: paths are raw bytes with exactly four bytes escaped.

use std::fmt;

pub fn encode_into(path: &[u8], out: &mut Vec<u8>) {
    for &b in path {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            _ => out.push(b),
        }
    }
}

pub fn encode(path: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(path.len());
    encode_into(path, &mut out);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeError {
    pub pos: usize,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid escape sequence at byte {}", self.pos)
    }
}

impl std::error::Error for DecodeError {}

pub fn decode(enc: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut out = Vec::with_capacity(enc.len());
    let mut i = 0;
    while i < enc.len() {
        let b = enc[i];
        if b == b'\\' {
            let unescaped = match enc.get(i + 1) {
                Some(b'\\') => b'\\',
                Some(b't') => b'\t',
                Some(b'n') => b'\n',
                Some(b'r') => b'\r',
                _ => return Err(DecodeError { pos: i }),
            };
            out.push(unescaped);
            i += 2;
        } else {
            out.push(b);
            i += 1;
        }
    }
    Ok(out)
}

/// A path is valid for an index if it is a non-empty relative path with no
/// empty, `.` or `..` components.
pub fn validate_relative(path: &[u8]) -> Result<(), &'static str> {
    if path.is_empty() {
        return Err("empty path");
    }
    if path[0] == b'/' {
        return Err("absolute path");
    }
    for comp in path.split(|&b| b == b'/') {
        match comp {
            b"" => return Err("empty path component"),
            b"." | b".." => return Err("path contains . or .. component"),
            _ => {}
        }
    }
    Ok(())
}

/// Lossy rendering for messages and reports only; never used as a data path.
pub fn display(path: &[u8]) -> String {
    String::from_utf8_lossy(&encode(path)).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_exactly_four_bytes() {
        let raw = b"a\\b\tc\nd\re\x00\x7f\xff/x";
        let enc = encode(raw);
        assert_eq!(&enc[..], b"a\\\\b\\tc\\nd\\re\x00\x7f\xff/x");
        assert_eq!(decode(&enc).as_deref(), Ok(&raw[..]));
    }

    #[test]
    fn rejects_unknown_escapes() {
        assert_eq!(decode(b"a\\x"), Err(DecodeError { pos: 1 }));
        assert_eq!(decode(b"trailing\\"), Err(DecodeError { pos: 8 }));
    }

    #[test]
    fn validates_relative_paths() {
        assert!(validate_relative(b"a/b").is_ok());
        assert!(validate_relative(b"").is_err());
        assert!(validate_relative(b"/a").is_err());
        assert!(validate_relative(b"a//b").is_err());
        assert!(validate_relative(b"a/").is_err());
        assert!(validate_relative(b"a/../b").is_err());
        assert!(validate_relative(b"./a").is_err());
    }
}
