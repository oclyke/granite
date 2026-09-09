# Decisions

Places where IMPLEMENTATION.md was silent, ambiguous, or could not be
satisfied literally, and what was chosen.

## Deviations

**Collision-renamed paths are canonicalised on read.** The spec's rename rule
("rename every claimant") loses the information that `notes~18ff5e4a.txt`
once claimed `notes.txt`, so a second merge would treat it as a distinct
path and `merge(merge(A,B),C)` would differ from `merge(A,B,C)` whenever
C also claims `notes.txt`. Property 2 cannot hold without an inverse rule.
So: a final component whose stem ends in `~` plus 8, 16 or 64 hex characters
that are a prefix of the record's own hash is treated as claiming the
unsuffixed path. Only a prior granite merge produces such a name. Plan
counts materialised paths after canonicalisation, so plan still matches
merge. Documented in FORMAT.md §10.1.

**Names the filesystem aliases are a merge failure, not an "existing" hit.**
APFS folds Unicode normalisation and, by default, case. Writing `café`
(NFD) after `café` (NFC) finds the NFC file already there. Treating that as
"already present" would put a name in the output index that `readdir` never
returns, and `verify` would then report it missing. Merge therefore checks
that the parent directory listing contains the exact name bytes before
accepting an existing file, and otherwise reports the entry (exit 3) without
writing. Discovery at merge time is what the spec asks for; this just makes
the discovery loud and keeps the output index truthful.

**Copy verification hashes the stream as it is written, not a re-read.** A
re-read immediately after writing is served from the page cache and proves
nothing about the medium, which is the storage layer's job by axiom 4. What
the hash does prove is that the source bytes matched the index, which is the
realistic failure (a stale index). Hardlinks created with `--link` are
re-read because no bytes passed through granite. Dedupe links between paths
of one hash within the output share an already-verified inode and are not
re-read.

**Merged files get a plain `fsync`, with a device-cache flush every 1000
files or 5 seconds.** std's `sync_all` and `sync_data` both issue
`F_FULLFSYNC` on macOS, about 6 ms per call, which made a merge of 12,000
small files take 85 seconds; a plain `fsync` (via `rustix`, keeping the
crate free of `unsafe`) is 0.2 ms and still orders the data before the
rename. The periodic `F_FULLFSYNC` bounds what a power loss can take back to
one batch of already-renamed files; those show up on the re-run as "exists
with different content" and must be removed by hand. Linux has no separate
cache-flush call, so there `fsync` is the full operation.

**`--index-dir` is also accepted by `merge` and `verify`.** Otherwise a shard
scanned from read-only media could never be merged or verified. `--name`
applies only to `scan` and `verify`; merge uses the default name for each
source.

**`sort -c` applies to the record region.** The header cannot be in sorted
order (`producer` sorts after `hash`). The CLI test strips the header before
checking.

**The output directory may not contain a source either.** The spec forbids
merging into a directory inside a source; the reverse (a source inside the
output) is also refused, since the output's index would then describe files
the merge did not write.

## Ambiguities resolved

- **Empty directories count as skipped** and so make `scan` exit 2, the same
  as any other entry that is not in the index.
- **`verify` exit code**: 3 for any missing, unindexed or changed file; 2 if
  the only problem was unreadable files; symlinks and other non-regular
  entries are ignored by verify because the index never contains them.
- **`ratio`** in `plan` is `naive_bytes / deduplicated_bytes`, printed as a
  multiplier, `null`/`n/a` when there is nothing to divide.
- **Per-file gate subsumes the cross-input check.** Every reader refuses any
  `hash` or `path_encoding` other than the one value v0 defines, so two
  accepted inputs necessarily agree. The check exists in one place
  (`IndexHeader::check`) instead of two.
- **Same hash, different size across inputs** is a corrupt index (or a hash
  collision) and aborts `plan`/`merge` with exit 1.
- **`hash` layout `paths` index** lives at `DIR/.granite/paths` rather than
  in the data tree, so `verify` on the output stays clean.
- **Refusal messages** follow the spec's wording and add a line naming the
  offending file, since `plan` and `merge` take many inputs.
- **Foreign journal** (different producer) is discarded with a notice and a
  full scan follows, rather than an error the user must clear by hand. A
  torn final line is dropped; the journal is truncated to its last complete
  record before appending.
- **Paths in human and JSON output** are shown in `escaped-bytes-v1` form,
  lossily converted to UTF-8. This is display only; no path ever passes
  through a `String` on a data path.
- **Copies preserve mode bits and mtime** from the source. Neither is
  identity, but an archive of photos is friendlier with its dates intact.
- **Temp space**: `scan` and `merge` sort in the directory that receives the
  index. `plan` and `verify` sort in the system temp dir (`TMPDIR`) because
  their inputs may be read-only. Set `TMPDIR` if the default is small.
- **Hints and the resume set are held in memory** (a hash map keyed by path).
  The external sort keeps the index build out of RAM as required; the hint
  map is roughly 100 bytes per file, which is acceptable at ten million files.
- **No progress output** during scan or merge. The journal's line count is
  the progress indicator for now.
- **A file that changes type between the walk and the hash** (regular file
  replaced by a symlink) is recorded in `skipped`.
- **YAML crate**: `yaml-rust2` (0.12, maintained under the saphyr project).
  `serde_yaml` is archived; `serde_yml` was avoided on maintenance grounds.
  Headers are hand-emitted as the spec requires and only parsed with the
  crate.
- **Rust edition 2024**, since the toolchain and the existing manifest
  already used it; the spec asked for 2021 or later.
- **`--dedupe-links`** takes an explicit boolean (`--dedupe-links false`)
  rather than a `--no-` twin, keeping one flag.
- **`mtime_nanos`** in hints is signed to represent pre-1970 timestamps.
