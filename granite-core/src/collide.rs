//! Path-collision renaming (§7.3) and its inverse.
//!
//! A merge renames every claimant of a contested path to `stem~<hex>.ext`.
//! For merges to compose (`merge(merge(A, B), C) == merge(A, B, C)`), a later
//! merge must recognise such a name as still claiming `stem.ext`: a final
//! component whose `~` suffix is a hex prefix of the record's own hash is
//! canonicalised back before collisions are computed. Nothing but a prior
//! granite merge produces a name that satisfies that check.

use crate::hex;

const PREFIX_LENGTHS: [usize; 3] = [8, 16, 64];

/// Shortest of 8, 16, 64 hex characters that keeps every hash distinct.
pub(crate) fn prefix_needed(hashes: &[[u8; 32]]) -> usize {
    PREFIX_LENGTHS
        .into_iter()
        .find(|&n| {
            let mut prefixes: Vec<&[u8]> = hashes.iter().map(|h| &h[..n / 2]).collect();
            prefixes.sort_unstable();
            prefixes.windows(2).all(|w| w[0] != w[1])
        })
        .unwrap_or(64)
}

fn split_name(path: &[u8]) -> (usize, usize) {
    let name_start = path.iter().rposition(|&b| b == b'/').map_or(0, |i| i + 1);
    let name = &path[name_start..];
    let ext_at = name
        .iter()
        .rposition(|&b| b == b'.')
        .filter(|&i| i > 0)
        .unwrap_or(name.len());
    (name_start, name_start + ext_at)
}

/// `dir/IMG_001.jpg` -> `dir/IMG_001~1e0f9c3a.jpg`; no extension -> suffix appended.
pub(crate) fn renamed(path: &[u8], hash: &[u8; 32], n: usize) -> Vec<u8> {
    let (_, ext_at) = split_name(path);
    let mut out = Vec::with_capacity(path.len() + n + 1);
    out.extend_from_slice(&path[..ext_at]);
    out.push(b'~');
    out.extend_from_slice(&hex::encode(hash).as_bytes()[..n]);
    out.extend_from_slice(&path[ext_at..]);
    out
}

/// Strips a `~<hex>` suffix that a prior merge added for this hash, if present.
pub(crate) fn canonical(path: &[u8], hash: &[u8; 32]) -> Option<Vec<u8>> {
    let (name_start, ext_at) = split_name(path);
    let stem = &path[name_start..ext_at];
    let tilde = stem.iter().rposition(|&b| b == b'~').filter(|&i| i > 0)?;
    let suffix = &stem[tilde + 1..];
    if !PREFIX_LENGTHS.contains(&suffix.len())
        || hex::encode(hash).as_bytes()[..suffix.len()] != *suffix
    {
        return None;
    }
    let mut out = Vec::with_capacity(path.len());
    out.extend_from_slice(&path[..name_start + tilde]);
    out.extend_from_slice(&path[ext_at..]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: [u8; 32] = [
        0x1e, 0x0f, 0x9c, 0x3a, 0xaa, 0xbb, 0xcc, 0xdd, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];

    #[test]
    fn rename_placement_and_inverse() {
        for (path, n, want) in [
            (&b"a/IMG_001.jpg"[..], 8, &b"a/IMG_001~1e0f9c3a.jpg"[..]),
            (b"README", 8, b"README~1e0f9c3a"),
            (b"d.ir/.bashrc", 8, b"d.ir/.bashrc~1e0f9c3a"),
            (b"x.tar.gz", 16, b"x.tar~1e0f9c3aaabbccdd.gz"),
        ] {
            assert_eq!(renamed(path, &H, n), want);
            assert_eq!(canonical(want, &H).as_deref(), Some(path));
        }
        assert_eq!(canonical(b"a/IMG_001.jpg", &H), None);
        assert_eq!(canonical(b"a~deadbeef.jpg", &H), None);
        assert_eq!(canonical(b"a~1e0f9c.jpg", &H), None);
        assert_eq!(canonical(b"~1e0f9c3a.jpg", &H), None);
    }

    #[test]
    fn prefix_grows_when_needed() {
        let a = [0u8; 32];
        let mut b = [0u8; 32];
        assert_eq!(prefix_needed(&[a, [1; 32]]), 8);
        b[4] = 1;
        assert_eq!(prefix_needed(&[a, b]), 16);
        b = [0u8; 32];
        b[31] = 1;
        assert_eq!(prefix_needed(&[a, b]), 64);
    }
}
