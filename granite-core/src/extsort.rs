//! External sort of byte lines: buffer up to a memory budget, spill sorted runs
//! to temp files, k-way merge. Output is strictly ascending (duplicates dropped).

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use tempfile::TempDir;

pub const DEFAULT_BUDGET: usize = 256 << 20;

pub(crate) struct Sorter {
    budget: usize,
    tmp: TempDir,
    runs: Vec<File>,
    buf: Vec<Vec<u8>>,
    used: usize,
}

impl Sorter {
    pub fn new(tmp_in: &Path, budget: usize) -> io::Result<Self> {
        Ok(Sorter {
            budget,
            tmp: tempfile::Builder::new()
                .prefix(".granite-sort.")
                .tempdir_in(tmp_in)?,
            runs: Vec::new(),
            buf: Vec::new(),
            used: 0,
        })
    }

    pub fn push(&mut self, line: Vec<u8>) -> io::Result<()> {
        self.used += line.len() + std::mem::size_of::<Vec<u8>>();
        self.buf.push(line);
        if self.used >= self.budget {
            self.spill()?;
        }
        Ok(())
    }

    fn spill(&mut self) -> io::Result<()> {
        self.buf.sort_unstable();
        let file = tempfile::tempfile_in(self.tmp.path())?;
        {
            let mut w = BufWriter::with_capacity(1 << 20, &file);
            for line in self.buf.drain(..) {
                w.write_all(&line)?;
                w.write_all(b"\n")?;
            }
            w.flush()?;
        }
        self.runs.push(file);
        self.used = 0;
        Ok(())
    }

    pub fn finish(mut self) -> io::Result<Sorted> {
        if self.runs.is_empty() {
            self.buf.sort_unstable();
            self.buf.dedup();
            return Ok(Sorted {
                mem: std::mem::take(&mut self.buf).into_iter(),
                runs: Vec::new(),
                heap: BinaryHeap::new(),
                pending: None,
                _tmp: self.tmp,
            });
        }
        if !self.buf.is_empty() {
            self.spill()?;
        }
        let mut runs = Vec::with_capacity(self.runs.len());
        let mut heap = BinaryHeap::with_capacity(self.runs.len());
        for (i, file) in self.runs.drain(..).enumerate() {
            let mut r = BufReader::with_capacity(1 << 20, reopen(&file)?);
            if let Some(line) = next_line(&mut r)? {
                heap.push(Reverse((line, i)));
            }
            runs.push(r);
        }
        Ok(Sorted {
            mem: Vec::new().into_iter(),
            runs,
            heap,
            pending: None,
            _tmp: self.tmp,
        })
    }
}

fn reopen(file: &File) -> io::Result<File> {
    use std::io::Seek;
    let mut f = file.try_clone()?;
    f.seek(io::SeekFrom::Start(0))?;
    Ok(f)
}

fn next_line(r: &mut impl BufRead) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    if r.read_until(b'\n', &mut line)? == 0 {
        return Ok(None);
    }
    line.pop();
    Ok(Some(line))
}

pub(crate) struct Sorted {
    mem: std::vec::IntoIter<Vec<u8>>,
    runs: Vec<BufReader<File>>,
    heap: BinaryHeap<Reverse<(Vec<u8>, usize)>>,
    pending: Option<Vec<u8>>,
    _tmp: TempDir,
}

impl Sorted {
    /// Yields one line behind the merge so that the line held back for the
    /// duplicate check is the same allocation later handed to the caller;
    /// comparing against a retained copy would clone every record.
    fn next_merged(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let Some(Reverse((line, i))) = self.heap.pop() else {
                return Ok(self.pending.take());
            };
            if let Some(next) = next_line(&mut self.runs[i])? {
                self.heap.push(Reverse((next, i)));
            }
            if self.pending.as_deref() == Some(&line[..]) {
                continue;
            }
            if let Some(out) = self.pending.replace(line) {
                return Ok(Some(out));
            }
        }
    }
}

impl Iterator for Sorted {
    type Item = io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.runs.is_empty() {
            return self.mem.next().map(Ok);
        }
        self.next_merged().transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spills_and_merges_with_dedup() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Sorter::new(dir.path(), 64).unwrap();
        let lines: Vec<Vec<u8>> = (0..200)
            .map(|i| format!("k{:03}", (i * 37) % 100).into_bytes())
            .collect();
        for l in &lines {
            s.push(l.clone()).unwrap();
        }
        let out: Vec<Vec<u8>> = s.finish().unwrap().collect::<io::Result<_>>().unwrap();
        let mut expect = lines;
        expect.sort();
        expect.dedup();
        assert_eq!(out, expect);
        assert_eq!(out.len(), 100);
    }
}
