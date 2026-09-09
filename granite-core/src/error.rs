use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("{}: line {line}: {msg}", path.display())]
    Malformed {
        path: PathBuf,
        line: u64,
        msg: String,
    },
    /// Format-version or semantic-key refusal (exit code 4).
    #[error("{msg}")]
    Refused { msg: String },
    #[error("{0}")]
    Usage(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Refused { .. } => 4,
            _ => 1,
        }
    }
}

pub(crate) trait IoContext<T> {
    fn at(self, path: &Path) -> Result<T>;
}

impl<T> IoContext<T> for io::Result<T> {
    fn at(self, path: &Path) -> Result<T> {
        self.map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })
    }
}
