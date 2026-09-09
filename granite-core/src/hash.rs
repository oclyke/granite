use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

pub(crate) const BUF_SIZE: usize = 1 << 20;

/// Below this, blake3's multithreaded update costs more than it saves.
const RAYON_THRESHOLD: usize = 128 * 1024;

fn update(hasher: &mut blake3::Hasher, chunk: &[u8]) {
    if chunk.len() >= RAYON_THRESHOLD {
        hasher.update_rayon(chunk);
    } else {
        hasher.update(chunk);
    }
}

/// Hashes everything readable from `r`; `sink` receives each chunk in order.
pub(crate) fn hash_stream(
    r: &mut impl Read,
    buf: &mut [u8],
    mut sink: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<([u8; 32], u64)> {
    let mut hasher = blake3::Hasher::new();
    let mut total = 0u64;
    loop {
        let n = r.read(buf)?;
        if n == 0 {
            break;
        }
        update(&mut hasher, &buf[..n]);
        sink(&buf[..n])?;
        total += n as u64;
    }
    Ok((*hasher.finalize().as_bytes(), total))
}

pub(crate) fn hash_file(path: &Path, buf: &mut [u8]) -> io::Result<([u8; 32], u64)> {
    let mut f = File::open(path)?;
    hash_stream(&mut f, buf, |_| Ok(()))
}

pub(crate) fn copy_hashing(
    src: &Path,
    dst: &mut impl Write,
    buf: &mut [u8],
) -> io::Result<([u8; 32], u64)> {
    let mut f = File::open(src)?;
    hash_stream(&mut f, buf, |chunk| dst.write_all(chunk))
}
