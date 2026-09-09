# granite v0 — implementation spec

You are implementing `granite`, a Rust CLI. Read this whole document before writing code.
Where this spec is explicit, follow it exactly — especially the on-disk format, which is a
long-term compatibility surface. Where it is silent, use judgement and leave a comment
explaining the choice.

This is alpha software that will later be depended on heavily for irreplaceable data.
Correctness, determinism, and format discipline matter more than features or speed.

---

## 1. What granite is

granite merges N partial copies ("shards") of a file collection into one deduplicated
collection. Shards are ordinary directory trees on ordinary filesystems. Output is
ordinary files. granite never owns a repository format for the data itself.

The design follows a fixed set of axioms. Do not "helpfully" violate them:

1. Different bytes means different files. No fuzzy or perceptual matching, ever.
2. No deletion. The merge is a monotone union. There are no tombstones.
3. Paths, timestamps, and filesystem metadata are **not** identity. Only the content hash
   is identity. Paths are annotations attached to a hash.
4. Storage reliability is a lower layer's job (ZFS, RAID, etc). granite does not repair.
5. Sidecar files (XMP, .aae, RAW+JPEG pairs) are user-space concerns. granite treats them
   as ordinary independent files.
6. Indexes are a cache, not truth. Truth is always the bytes on disk.
7. No drive IDs, no "best copy", no provenance stored on disk.

Consequence: the data model is `hash -> {size, set of paths}`, and a merge is a set union.

---

## 2. Scope of v0

Implement exactly these commands:

- `granite scan` — walk a shard, hash every regular file, write an index into the shard.
- `granite plan` — from indexes alone, report deduplicated size and inter-shard overlap.
- `granite merge` — materialize the union of N shards into an output directory.
- `granite verify` — re-hash a shard and diff against its index.
- `granite fingerprint` — print the fingerprint of a shard's index.

Explicitly **out of scope for v0** (do not implement, do not stub elaborately):

- `fill` (pushing content back out to partial drives)
- compression, encryption, network, remotes
- Windows support (v0 targets Linux and macOS only)
- HDD-aware IO scheduling and physical-placement ordering
- symlink, hardlink-detection, empty-directory, or extended-attribute preservation
- any GUI, daemon, or watch mode

---

## 3. On-disk layout

Per shard, in a `.granite/` directory at the shard root:

```
.granite/index          canonical, sorted, deterministic       (committed artifact)
.granite/hints          rescan-skip cache                      (disposable)
.granite/scan.meta      provenance of the last scan            (disposable)
.granite/scan.journal   present only during / after a failed scan
.granite/skipped        log of non-regular or unreadable entries (informational)
```

The split is load-bearing. **`.granite/index` must be a pure function of the shard's
content and relative paths.** No timestamps, no hostnames, no drive names, no scan
duration, no absolute paths. Two drives holding identical files at identical relative
paths must produce byte-identical `index` files. Everything volatile lives in `hints`
or `scan.meta`.

Anything that would break that property goes in `scan.meta`. If you are tempted to add a
field to the index header and it varies between two scans of the same content, you are
wrong; put it in `scan.meta`.

`--index-dir <DIR>` overrides the location for shards on read-only media. In that case the
files are written as `<DIR>/<name>.index` etc., where `<name>` is user-supplied via
`--name`, defaulting to a sanitised form of the shard's basename. Nothing about the index's
*content* may depend on where it is stored.

---

## 4. The index file format

### 4.1 Structure

A YAML header delimited by `---` lines, followed by TSV records:

```
---
granite_index_version: 0
producer: granite 0.1.0
hash: blake3-256
path_encoding: escaped-bytes-v1
record_fields: [hash, size, path]
sort: line-bytewise
requires: []
---
1e0f...9c	4718592	2019/trip/IMG_0431.CR2
1e0f...9c	4718592	sorted/canon/0431.CR2
3a71...02	12043	notes.txt
```

The file is bytes, not text. It is not required to be valid UTF-8 (paths may not be).

### 4.2 Header rules

- Opening `---` on line 1, closing `---` on its own line. Records begin immediately after.
- The header region must be valid UTF-8 and valid YAML: a single flat mapping whose values
  are scalars or flat sequences of scalars. No anchors, aliases, tags, nested maps,
  multi-document streams, or block scalars.
- **Write the header by hand**, not with a YAML emitter: emit keys in the fixed order above,
  one per line, `key: value`, so output is byte-stable across library versions. Read it with
  a maintained YAML crate. Note that `serde_yaml` is unmaintained/deprecated; pick a
  maintained alternative and verify its status at implementation time rather than trusting
  this document.
- Round-trip test: parse a header, re-emit it, assert byte equality.

Field meanings:

| key | meaning |
|---|---|
| `granite_index_version` | Format version of this file. Integer. `0` means alpha (see §5). |
| `producer` | Tool name and version that wrote it. Informational only, except during alpha. |
| `hash` | Hash function and output length. v0: `blake3-256`. Forever-decision. |
| `path_encoding` | Escaping scheme for the path field. v0: `escaped-bytes-v1`. |
| `record_fields` | Field order in records. Lets a future version add fields detectably. |
| `sort` | Record ordering guarantee. v0: `line-bytewise`. |
| `requires` | List of extension names the reader **must** understand (see §5.3). |

### 4.3 Record rules

`hash \t size \t path \n` — exactly, with no trailing whitespace and a final newline.

- `hash`: 64 lowercase hex characters (BLAKE3, 256-bit default output).
- `size`: decimal bytes, no leading zeros, no separators. `0` is valid.
- `path`: relative to the shard root, `/` as separator, no leading `/`, no `.` or `..`
  components. Encoded per `escaped-bytes-v1`.
- Sort order: **bytewise over the entire record line**, ascending, as `LC_ALL=C sort` would
  produce. Because the hash field is fixed-width and first, this is hash-major; because
  equal hashes imply equal sizes, the size field never perturbs ordering.
- No duplicate `(hash, path)` records. Same hash at many paths is normal and expected.
- Same path with two different hashes cannot occur *within* one index (a path holds one
  file). It routinely occurs *across* indexes, and that is the merge's central case.

### 4.4 `escaped-bytes-v1`

Paths are arbitrary byte sequences on Unix. Work with `&[u8]` / `OsStr` via
`std::os::unix::ffi::OsStrExt` throughout. Never round-trip a path through `String`
or `to_string_lossy` — a lossy conversion is silent data corruption here.

Escape exactly four bytes, and no others:

```
0x5C  \   ->  \\
0x09  TAB ->  \t
0x0A  LF  ->  \n
0x0D  CR  ->  \r
```

Every other byte, including 0x00–0x08, 0x7F, and all bytes ≥ 0x80, is emitted literally.
Decoding rejects a backslash followed by anything other than `\`, `t`, `n`, `r`.

**Do not normalise Unicode and do not case-fold at scan time.** NFC and NFD forms of the
same name are different paths in the index. They only conflict when materialised onto a
filesystem that considers them equal, and discovering that at merge time is correct
behaviour, not a bug.

### 4.5 Fingerprint

`fingerprint(index) = blake3-256(entire index file bytes, header included)`.

Printed as 64 lowercase hex chars. Because the file is deterministic, this identifies a
shard's content-and-paths exactly, and two shards can be compared by comparing 32 bytes.
Anything that breaks fingerprint stability across copies of the same content is a bug of
the highest severity.

Note the fingerprint covers `producer`, so during alpha it changes when the version does.
That is intended (§5.1).

---

## 5. Versioning strategy

This is the part most likely to be gotten wrong later, so it is specified tightly now.

### 5.1 Alpha: version 0

While `granite_index_version` is `0`, the format is explicitly unstable. Readers apply an
extra rule: **an index with version 0 is only accepted if its `producer` string exactly
matches the reading binary's own `producer` string.** Mismatch is a hard error:

```
error: index was written by granite 0.1.0, this is granite 0.2.0
       index format version 0 is unstable; formats from different alpha builds
       are not interchangeable
hint:  re-run `granite scan --force` on this shard
```

This makes the alpha period safe: you can change the format freely without ever silently
misreading an old index, at the cost of forced rescans. Do not weaken this to a warning.

When the format is declared stable, bump to `granite_index_version: 1` and delete the
producer-matching rule (a `#[cfg]`-free, single-site change — write it that way).

### 5.2 Post-1.0 reader policy

- Version equal to the reader's: accept.
- Version **greater** than the reader's: refuse, with a message naming both versions.
- Version **less** than the reader's: accept read-only if a documented reader exists for
  that version; otherwise refuse. Never upgrade a file in place implicitly. Migration is
  an explicit future `granite migrate` command that writes a new file and leaves the old
  one alone.
- Unrecognised header keys not named in `requires`: ignore silently. This is the additive
  forward-compatibility path.

### 5.3 The `requires` mechanism

A future version that adds a change a naive reader would *misinterpret* (rather than merely
miss) lists a name in `requires`. Any reader that does not recognise every name in
`requires` must refuse the file, whatever the version number says. Implement the check now
even though v0 always emits `requires: []`; it costs five lines and cannot be retrofitted.

Example of when it would be used: adding a `deleted` flag column would change the meaning
of existing records, so it would ship as `requires: [tombstones]`.

### 5.4 Semantic keys are not the format version

`hash` and `path_encoding` are checked independently of `granite_index_version`. Two
indexes with different `hash` values must never be merged, compared, or unioned, even at
the same format version — refuse with a clear message. The version says "how to parse";
these say "what the identities mean".

---

## 6. `hints` and `scan.meta`

### 6.1 `.granite/hints`

Same header-plus-TSV shape, `granite_hints_version: 0`, records:

```
path \t size \t mtime_nanos \t hash
```

sorted bytewise by line, path first. Purely a rescan accelerator: on scan, if a file's path,
size, and mtime all match a hint, reuse the recorded hash instead of reading the file.

Constraints:
- The merge, plan, verify, and fingerprint paths must **never** read `hints`. Enforce this
  structurally — put hints access behind a type that the merge code has no access to, or at
  minimum a module boundary with a comment saying why.
- `--force` on scan ignores hints entirely and re-reads every byte.
- A missing, malformed, or version-mismatched hints file is never fatal. Log and full-scan.

### 6.2 `.granite/scan.meta`

A YAML document (no records section). Everything volatile lives here:

```yaml
granite_meta_version: 0
producer: granite 0.1.0
started_at: 2026-09-09T14:02:11Z
finished_at: 2026-09-09T18:41:57Z
complete: true
files_indexed: 412903
bytes_indexed: 8814027391244
files_skipped: 17
index_fingerprint: 4c1a...ff
```

Informational and diagnostic. No command's behaviour may depend on it, with one exception:
`verify` may print `started_at` for context.

---

## 7. Commands

### 7.1 `granite scan <ROOT> [--index-dir D] [--name N] [--force] [--jobs N]`

1. If `.granite/scan.journal` exists, the previous scan was interrupted. Load it, treat its
   paths as already done, and resume. Print that this is happening.
2. Walk `ROOT` recursively, skipping `.granite/` itself. Follow no symlinks.
3. For each **regular file**: consult hints (unless `--force`), else BLAKE3 it. Append a
   record to `scan.journal` immediately. Fsync the journal periodically (every ~1000
   records or ~5s; make it a constant with a comment, not a magic number).
4. Non-regular entries (symlinks, FIFOs, sockets, devices) and empty directories are
   recorded in `.granite/skipped` with a reason, and are **not** in the index. They have no
   content identity under the axioms.
5. Unreadable files (permissions, IO errors) go to `skipped` with the error, and the scan
   continues. A single bad file must never abort a multi-hour scan.
6. On completion: external-sort the journal into the canonical index, write `hints` and
   `scan.meta`, then delete the journal. The journal's absence is what marks a scan
   complete — do not rely on `complete: true` in scan.meta.
7. Every output file is written to a temp file in the same directory, fsynced, then renamed
   over the target. Fsync the containing directory afterwards.

External sort: chunk the journal into memory-sized runs (default budget ~256 MB, flag-able),
sort each in memory, write to temp files, then k-way merge. Do not assume the index fits in
RAM — ten million records will be common.

Zero-byte files are ordinary files. They all share one hash. This is correct; do not special-case it.

### 7.2 `granite plan <TARGET>... [--json]`

Each `TARGET` is a shard root (reads `<root>/.granite/index`) or a path to an index file
directly. **Accepting bare index files is required, not optional** — the whole point is
answering questions about a collection with none of the drives plugged in.

Reads only indexes. Touches no data files. Refuses if `hash` or `path_encoding` differ
across inputs.

Streaming N-way merge over the sorted inputs, O(number of inputs) memory. Reports:

- Per shard: file count, byte total, distinct hash count.
- Union: distinct hash count, **deduplicated byte total** (sum of size over distinct
  hashes), naive total, bytes saved, ratio.
- Per shard: hashes present only in this shard and their byte total. This is the "which
  drive can I not afford to lose" number.
- Distribution of replica counts: how many hashes appear in exactly 1, 2, ... N shards,
  with byte totals.
- Pairwise overlap between shards: shared hash count and shared bytes.
- **Path collisions**: paths claimed by more than one distinct hash across the inputs, with
  a count and a sample. This is the only case requiring a rule, so surface it prominently.
- Materialised path count (total distinct `(hash, path)` pairs), which is what the output
  tree will actually contain.

`--json` emits the same data as a single JSON object for scripting. Human output goes to
stdout as plain aligned text; do not use colour or unicode box drawing.

Pairwise overlap is O(N²) in shards. That is fine for N in the tens. If N exceeds ~50,
print the summary and replica distribution but omit the pairwise matrix unless
`--pairwise` is given.

### 7.3 `granite merge <TARGET>... --into <DIR> [--layout paths|hash] [--link] [--dry-run]`

Materialises the union. Requires shard roots (not bare index files) for the sources it must
read bytes from.

**Layout `paths` (default).** Every `(hash, path)` pair in the union is emitted at `path`
under `DIR`. One hash appearing at five paths produces five entries — cheap by axiom 1, and
it is the only rule that requires no preference between paths.

Path collision rule, when a single path is claimed by more than one hash:

> **All** colliding entries are renamed, including the lexicographically first. Insert
> `~` plus the first 8 hex characters of the hash before the final extension:
> `IMG_001.jpg` → `IMG_001~1e0f9c3a.jpg`. For a name with no extension, append the suffix.
> If 8 characters are not enough to disambiguate (they will be, but check), extend to 16,
> then to the full hash.

Renaming *all* participants rather than "the losers" is what makes the output independent
of argument order. Do not optimise this away.

**Layout `hash`.** Files at `DIR/objects/<first2>/<hash>`, plus a `paths` index mapping
hashes to their path set. No collisions possible. Provide it because it is trivial once the
data model is right and it is the safer choice for a pure archive.

Other requirements:

- `--link` uses hardlinks when source and destination are on the same device, falling back
  to copy with a warning. In `paths` layout, multiple paths for one hash should hardlink to
  each other when possible even without `--link`; make that `--dedupe-links`, default on,
  with graceful fallback.
- Every write is hash-verified after the fact. A copy whose result does not hash to the
  expected value is an error: leave the temp file, do not rename it into place, report, and
  continue with other files.
- Resumable: if a target path already exists and hashes correctly, skip it. This makes
  re-running merge after a crash cheap and makes merge idempotent.
- The output directory gets its own `.granite/index` describing what was written, so it is
  itself a valid shard.
- `--dry-run` prints exactly what `plan` prints plus the list of renames, and writes nothing.
- Refuse to merge into a directory that is inside any source shard.

### 7.4 `granite verify <ROOT> [--jobs N]`

Full re-hash of the shard, ignoring hints. Diffs against `.granite/index` and reports:
files present on disk but not in the index, files in the index but missing on disk, and
files whose hash no longer matches. Exit code 3 if any mismatch. This is the command that
makes "index is cache, not truth" operationally real, so it must be as fast as scan.

### 7.5 `granite fingerprint <TARGET>...`

Prints `<fingerprint>  <target>` per line. Used to answer "have I already merged this
drive" without reading any data.

---

## 8. Correctness properties to test

These are requirements, not suggestions. Use `proptest` for the property tests and
`assert_cmd` + `tempfile` for the CLI tests.

1. **Merge is order-independent.** `merge(A, B, C)` and any permutation produce
   byte-identical output trees, including collision-renamed filenames.
2. **Merge is associative.** `merge(merge(A, B), C) == merge(A, B, C)`.
3. **Merge is idempotent.** `merge(X, output_of_merge(X)) == output_of_merge(X)`, and
   re-running merge into an existing output writes nothing new.
4. **Index is deterministic.** Scanning the same tree twice, and scanning a copy of a tree
   at a different absolute path, produce identical index bytes and identical fingerprints.
5. **Hints are inert.** For any tree, a scan with hints present and a scan with
   `--force` produce identical index bytes.
6. **Path encoding round-trips.** For arbitrary byte vectors excluding NUL and `/`,
   `decode(encode(p)) == p`. Include the four escaped bytes, high bytes, invalid UTF-8
   sequences, and NFC/NFD pairs of the same visual name as explicit fixture cases.
7. **Plan matches merge.** The deduplicated byte total and materialised path count reported
   by `plan` exactly match what `merge` writes, on every fixture.
8. **Interrupted scan resumes correctly.** Kill a scan mid-journal; the resumed scan's index
   is identical to an uninterrupted one.
9. **Sort order.** Every emitted index passes `LC_ALL=C sort -c`.
10. **Version gate.** An index with a higher version, an unknown `requires` entry, a
    different `hash`, or (during alpha) a different `producer` is refused by every command
    that reads indexes.

Fixture trees should include: an empty tree, multiple zero-byte files, a file at many paths,
one path with different content across shards, NFC/NFD filename pairs, a filename containing
a literal newline and a tab, a filename that is invalid UTF-8, deeply nested paths, and a
tree with no overlap at all with another.

---

## 9. Implementation notes

- Rust 2021 or later. Workspace with a `granite-core` library crate and a `granite` binary
  crate; all logic in the library, the binary only does argument parsing and output
  formatting.
- Crates: `blake3`, `clap` (derive), `serde` + `serde_json`, a maintained YAML crate,
  `walkdir` or `jwalk`, `rayon` or a bounded thread pool, `tempfile`, `anyhow` at the
  binary boundary with `thiserror` types in the library, `proptest`, `assert_cmd`.
  Check current versions and maintenance status yourself; do not trust versions from memory.
- Hashing parallelism: default `--jobs` to `min(4, available_parallelism())`. BLAKE3's own
  internal `update_rayon` on large files plus a few concurrent files is enough for v0.
  Reading multiple files concurrently from one spinning disk can be *slower*; do not
  parallelise aggressively by default and leave a comment saying why.
- Buffered reads with a 1 MiB buffer or `blake3::Hasher::update_mmap_rayon` where mmap is
  safe. Be aware mmap on a file being concurrently modified is UB-adjacent; prefer plain
  reads unless you gate mmap behind a flag.
- Exit codes: `0` success; `1` usage or argument error; `2` completed with skipped or
  unreadable files; `3` verification or hash mismatch; `4` format version or semantic-key
  refusal. Document these in `--help`.
- No global mutable state. No `unwrap()` outside tests. No path handling via `String`.
- `#![forbid(unsafe_code)]` in `granite-core` unless mmap forces an exception, in which case
  isolate it in one module.

---

## 10. Deliverables

1. The workspace, building clean with `cargo clippy -- -D warnings`.
2. All tests in §8 passing.
3. `FORMAT.md` — a standalone normative description of the index, hints, and meta formats,
   written so a third party could implement a compatible reader without seeing the Rust.
   Include the versioning rules from §5 verbatim in spirit. This document is the artifact
   that has to survive; treat it as the real deliverable.
4. `README.md` — what it is, the axioms, the five commands with worked examples.
5. A short `DECISIONS.md` noting anything where you deviated from this spec or where the
   spec was ambiguous, and what you chose.

Build in this order, with tests at each step: format read/write and fingerprint → scan →
plan → verify → merge. Do not start `merge` until `plan`'s numbers are trusted, because
`plan` is how `merge` gets validated.
