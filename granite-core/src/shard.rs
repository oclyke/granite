//! Where a shard's granite files live (§3).

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub const GRANITE_DIR: &str = ".granite";

#[derive(Debug, Clone)]
pub struct Layout {
    pub dir: PathBuf,
    pub index: PathBuf,
    pub hints: PathBuf,
    pub meta: PathBuf,
    pub journal: PathBuf,
    pub skipped: PathBuf,
}

impl Layout {
    pub fn in_shard(root: &Path) -> Layout {
        let dir = root.join(GRANITE_DIR);
        Layout {
            index: dir.join("index"),
            hints: dir.join("hints"),
            meta: dir.join("scan.meta"),
            journal: dir.join("scan.journal"),
            skipped: dir.join("skipped"),
            dir,
        }
    }

    pub fn external(dir: &Path, name: &str) -> Layout {
        Layout {
            index: dir.join(format!("{name}.index")),
            hints: dir.join(format!("{name}.hints")),
            meta: dir.join(format!("{name}.scan.meta")),
            journal: dir.join(format!("{name}.scan.journal")),
            skipped: dir.join(format!("{name}.skipped")),
            dir: dir.to_path_buf(),
        }
    }

    pub fn resolve(
        root: &Path,
        index_dir: Option<&Path>,
        name: Option<&str>,
    ) -> std::io::Result<Layout> {
        match index_dir {
            None => Ok(Layout::in_shard(root)),
            Some(dir) => {
                let name = match name {
                    Some(n) => n.to_owned(),
                    None => default_name(root)?,
                };
                Ok(Layout::external(dir, &name))
            }
        }
    }
}

/// The shard's basename with anything outside `[A-Za-z0-9._-]` replaced by `_`.
pub fn default_name(root: &Path) -> std::io::Result<String> {
    let canon = fs::canonicalize(root)?;
    let base = canon.file_name().map(|n| n.as_bytes()).unwrap_or(b"");
    let name: String = base
        .iter()
        .map(|&b| {
            if b.is_ascii_alphanumeric() || b"._-".contains(&b) {
                b as char
            } else {
                '_'
            }
        })
        .collect();
    Ok(if name.is_empty() {
        "shard".to_owned()
    } else {
        name
    })
}

/// A `plan`/`fingerprint` target: a shard root or a bare index file.
pub fn index_for_target(target: &Path) -> PathBuf {
    if target.is_dir() {
        Layout::in_shard(target).index
    } else {
        target.to_path_buf()
    }
}
