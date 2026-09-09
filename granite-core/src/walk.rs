//! Walks a shard and hashes its regular files on a small worker pool. Shared by
//! `scan` and `verify`.

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::UNIX_EPOCH;

use crate::error::{Error, IoContext, Result};
use crate::hash::{BUF_SIZE, hash_file};

pub(crate) enum Item {
    File {
        path: Vec<u8>,
        size: u64,
        mtime_nanos: i128,
        hash: [u8; 32],
    },
    Skipped {
        path: Vec<u8>,
        reason: String,
    },
}

/// A hash source consulted before reading a file (the hints cache, for `scan`).
pub(crate) type Precomputed<'a> = &'a (dyn Fn(&[u8], u64, i128) -> Option<[u8; 32]> + Sync);

pub(crate) struct Walk<'a> {
    pub root: &'a Path,
    /// Directories under `root` to leave out entirely, as paths relative to `root`.
    pub exclude: Vec<PathBuf>,
    pub jobs: usize,
    pub precomputed: Precomputed<'a>,
    pub already_done: &'a (dyn Fn(&[u8]) -> bool + Sync),
}

/// Default hashing concurrency. A few files in flight lets BLAKE3 overlap IO with
/// hashing; many files in flight makes a spinning disk seek instead of stream, so
/// the default stays low.
pub fn default_jobs() -> usize {
    thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(4)
}

pub(crate) fn mtime_nanos(meta: &fs::Metadata) -> i128 {
    match meta.modified() {
        Ok(t) => match t.duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_nanos() as i128,
            Err(e) => -(e.duration().as_nanos() as i128),
        },
        Err(_) => 0,
    }
}

fn relative(root: &Path, path: &Path) -> Vec<u8> {
    path.strip_prefix(root)
        .unwrap_or(path)
        .as_os_str()
        .as_bytes()
        .to_vec()
}

impl Walk<'_> {
    /// Calls `sink` on the main thread for every item, in no particular order.
    pub(crate) fn run(&self, mut sink: impl FnMut(Item) -> Result<()>) -> Result<()> {
        let jobs = self.jobs.max(1);
        let (work_tx, work_rx) = mpsc::sync_channel::<(Vec<u8>, PathBuf)>(jobs * 4);
        let work_rx = std::sync::Mutex::new(work_rx);
        let (item_tx, item_rx) = mpsc::sync_channel::<Item>(1024);

        thread::scope(|s| {
            let walker_tx = item_tx.clone();
            s.spawn(move || self.walk(work_tx, walker_tx));
            for _ in 0..jobs {
                let work_rx = &work_rx;
                let item_tx = item_tx.clone();
                let precomputed = self.precomputed;
                s.spawn(move || {
                    let mut buf = vec![0u8; BUF_SIZE];
                    loop {
                        let job = work_rx.lock().ok().and_then(|rx| rx.recv().ok());
                        let Some((rel, abs)) = job else { break };
                        let item = hash_one(rel, &abs, precomputed, &mut buf);
                        if item_tx.send(item).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(item_tx);
            for item in item_rx {
                sink(item)?;
            }
            Ok(())
        })
    }

    fn walk(&self, work_tx: mpsc::SyncSender<(Vec<u8>, PathBuf)>, item_tx: mpsc::SyncSender<Item>) {
        let root = self.root;
        let excluded: Vec<PathBuf> = self.exclude.iter().map(|e| root.join(e)).collect();
        let mut it = walkdir::WalkDir::new(root)
            .follow_links(false)
            .sort_by_file_name()
            .into_iter();
        while let Some(next) = it.next() {
            let entry = match next {
                Ok(e) => e,
                Err(e) => {
                    let path = e.path().map(|p| relative(root, p)).unwrap_or_default();
                    let reason = e
                        .io_error()
                        .map_or_else(|| e.to_string(), |io| io.to_string());
                    if item_tx.send(Item::Skipped { path, reason }).is_err() {
                        return;
                    }
                    continue;
                }
            };
            if entry.depth() == 0 {
                continue;
            }
            if excluded.iter().any(|x| x == entry.path()) {
                it.skip_current_dir();
                continue;
            }
            let rel = relative(root, entry.path());
            let ft = entry.file_type();
            let sent = if ft.is_file() {
                if (self.already_done)(&rel) {
                    continue;
                }
                work_tx.send((rel, entry.path().to_path_buf())).is_ok()
            } else if ft.is_dir() {
                match fs::read_dir(entry.path()).map(|mut d| d.next().is_none()) {
                    Ok(true) => item_tx
                        .send(Item::Skipped {
                            path: rel,
                            reason: "empty directory".into(),
                        })
                        .is_ok(),
                    Ok(false) => true,
                    Err(e) => item_tx
                        .send(Item::Skipped {
                            path: rel,
                            reason: e.to_string(),
                        })
                        .is_ok(),
                }
            } else {
                let reason = if ft.is_symlink() {
                    "symlink"
                } else {
                    "not a regular file"
                };
                item_tx
                    .send(Item::Skipped {
                        path: rel,
                        reason: reason.into(),
                    })
                    .is_ok()
            };
            if !sent {
                return;
            }
        }
    }
}

fn hash_one(rel: Vec<u8>, abs: &Path, precomputed: Precomputed<'_>, buf: &mut [u8]) -> Item {
    let result = (|| -> std::io::Result<Item> {
        let meta = fs::symlink_metadata(abs)?;
        if !meta.is_file() {
            return Ok(Item::Skipped {
                path: rel.clone(),
                reason: "changed to a non-regular file during scan".into(),
            });
        }
        // Metadata is captured before hashing so a modification during the read
        // makes the hint stale rather than wrongly fresh.
        let size = meta.len();
        let mtime_nanos = mtime_nanos(&meta);
        let hash = match precomputed(&rel, size, mtime_nanos) {
            Some(h) => h,
            None => hash_file(abs, buf)?.0,
        };
        Ok(Item::File {
            path: rel.clone(),
            size,
            mtime_nanos,
            hash,
        })
    })();
    result.unwrap_or_else(|e| Item::Skipped {
        path: rel,
        reason: e.to_string(),
    })
}

/// Resolves `dir` relative to `root` if it lies inside it, for exclusion.
pub(crate) fn relative_if_inside(root: &Path, dir: &Path) -> Result<Option<PathBuf>> {
    let root_c = fs::canonicalize(root).at(root)?;
    let Ok(dir_c) = fs::canonicalize(dir) else {
        return Ok(None);
    };
    Ok(dir_c.strip_prefix(&root_c).ok().map(Path::to_path_buf))
}

pub(crate) fn require_dir(root: &Path) -> Result<()> {
    if root.is_dir() {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "{} is not a directory",
            root.display()
        )))
    }
}
