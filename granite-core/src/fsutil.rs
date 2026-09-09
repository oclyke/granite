use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use tempfile::NamedTempFile;

/// Writes `target` via a temp file in the same directory: fill, fsync, rename,
/// then fsync the directory.
pub(crate) fn write_atomic<T>(
    target: &Path,
    fill: impl FnOnce(&mut BufWriter<&File>) -> io::Result<T>,
) -> io::Result<T> {
    let parent = target.parent().unwrap_or(Path::new("."));
    let tmp = NamedTempFile::with_prefix_in(".granite-tmp.", parent)?;
    let out = {
        let mut w = BufWriter::with_capacity(1 << 20, tmp.as_file());
        let out = fill(&mut w)?;
        w.flush()?;
        out
    };
    tmp.as_file().sync_all()?;
    tmp.persist(target)?;
    fsync_dir(parent)?;
    Ok(out)
}

pub(crate) fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Pushes a file's data to the storage device. On Apple platforms this is a
/// plain `fsync`, not the `F_FULLFSYNC` that std's `sync_all`/`sync_data`
/// issue: the latter flushes the drive's write cache and costs ~6 ms per call,
/// which would dominate a merge of many small files. Pair with a periodic
/// [`flush_device`] to bound what a power loss can lose.
pub(crate) fn fsync(file: &File) -> io::Result<()> {
    rustix::fs::fsync(file).map_err(io::Error::from)
}

/// Flushes the device write cache behind `file`. A no-op beyond `fsync` where
/// the platform has no separate notion of it.
pub(crate) fn flush_device(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    {
        rustix::fs::fcntl_fullfsync(file).map_err(io::Error::from)
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        rustix::fs::fsync(file).map_err(io::Error::from)
    }
}
