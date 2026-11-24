# granite

**permanent storage**

`granite` merges N partial copies ("shards") of a file collection into one
deduplicated collection. Shards are ordinary directory trees. Output is
ordinary files. There is no repository format for the data itself; granite
only ever adds a small `.granite/` directory holding an index.

An opinionated set of axioms makes the tool small and its behaviour easy to
predict:

axiom | consequence
---|---
storage is cheap | different bytes; different file. No fuzzy matching, ever.
no deletion | the merge is a monotone union. There are no tombstones.
file content is identity | paths, timestamps and metadata are annotations attached to a hash, not identity.
storage is reliable | bit rot is the storage layer's job (ZFS, RAID). granite does not repair.
indexes are a cache | truth is always the bytes on disk. `verify` re-derives it.
idempotent and order independent | `merge(merge(A,B),C) == merge(A,B,C)`, and re-running a merge writes nothing.

The data model that falls out is `hash -> {size, set of paths}`, and a merge
is a set union.

## Install

```
cargo install --path granite
```

Linux and macOS. Rust 2024 edition.

## Commands

### `granite scan <ROOT>`

Walks a shard, hashes every regular file with BLAKE3, and writes
`ROOT/.granite/index`. Symlinks, devices, empty directories and unreadable
files are listed in `.granite/skipped` and are not indexed.

```
$ granite scan /mnt/drive-a
indexed 412903 files, 8814027391244 bytes; skipped 17
fingerprint 4c1a0e7d…
see /mnt/drive-a/.granite/skipped for skipped entries
```

A second scan reuses `.granite/hints` to skip files whose path, size and
mtime are unchanged; `--force` re-reads every byte. An interrupted scan
leaves `.granite/scan.journal` and resumes from it on the next run.

For read-only media, keep the index elsewhere:

```
$ granite scan /Volumes/DVD_2014 --index-dir ~/granite-indexes
$ ls ~/granite-indexes
DVD_2014.index  DVD_2014.hints  DVD_2014.scan.meta  DVD_2014.skipped
```

### `granite plan <TARGET>...`

Answers "what would the union look like" from indexes alone. Targets are
shard roots or bare index files, so this works with no drives plugged in.

```
$ granite plan /mnt/drive-a ~/granite-indexes/DVD_2014.index
shard                                files             bytes    distinct   only here   bytes only here
/mnt/drive-a                        412903     8814027391244      401222       12000         123456789
~/granite-indexes/DVD_2014.index     40212      401220034711       40201       31007       300012345600

union
  distinct hashes               432423
  deduplicated bytes     8914039736844
  naive bytes            9215247425955
  bytes saved             301207689111
  ratio                           1.03x
  materialised paths            453115

replicas      hashes             bytes
  1           423229     8814027391244
  2             9194       99980345600

pairwise overlap      hashes             bytes
  [0] x [1]           9194       99980345600

path collisions: 1
  paths claimed by more than one file; merge --layout paths refuses until these are renamed
  2014/notes.txt
      18ff5e4a…  /mnt/drive-a/2014/notes.txt
      e3d6cfa0…  ~/granite-indexes/DVD_2014.index/2014/notes.txt
```

"Only here" is the number you cannot afford to lose. `--json` emits the same
data as one object. Above 50 shards the pairwise matrix is skipped unless
`--pairwise` is given.

### `granite merge <TARGET>... --into <DIR>`

Materialises the union. Every `(hash, path)` pair in the inputs is written
at `path` under `DIR`; a hash with five paths produces five entries,
hardlinked to each other where the filesystem allows.

```
$ granite merge /mnt/drive-a /mnt/drive-b --into /tank/photos
wrote 44120 files (401220034711 bytes), linked 3, already present 0, failed 0
fingerprint 9b31…
```

When one path is claimed by two different files, merge writes nothing and
lists each claimant with its location:

```
$ granite merge /mnt/drive-a /mnt/drive-b --into /tank/photos
error: 1 paths are claimed by more than one file; nothing was written

resolve by renaming all but one claimant of each path in its shard, then `granite scan` that shard:
  2014/notes.txt
      18ff5e4a…  /mnt/drive-a/2014/notes.txt
      e3d6cfa0…  /mnt/drive-b/2014/notes.txt
$ mv /mnt/drive-b/2014/notes.txt /mnt/drive-b/2014/notes-b.txt
$ granite scan /mnt/drive-b
$ granite merge /mnt/drive-a /mnt/drive-b --into /tank/photos
```

A path is only a location, not identity, so renaming changes nothing about
what the collection contains. `plan` shows collisions ahead of time, and
`--layout hash` cannot collide at all.

Every copy is hash-verified. A file whose bytes do not match its index entry
is reported and left as a temp file; the merge continues and exits 3. A
re-run skips anything already present and correct, so a crashed merge is
resumed by running it again.

`--dry-run` prints the plan, and the collision list if any, and writes nothing. `--link`
hardlinks from sources on the same device instead of copying. `--layout hash`
writes each distinct content once at `objects/<xx>/<hash>` with a
`.granite/paths` index mapping hashes to their paths; nothing can collide.

`DIR` gets its own `.granite/index`, so it is a shard and can be merged again.
If `DIR` already has an index, it is an implicit input: `merge B --into DIR`
is `merge DIR B --into DIR`, so drives can be added one at a time. Files
already in `DIR` are trusted rather than re-read, collisions against them are
reported like any other, and a new path whose content `DIR` already holds is
hardlinked to it. `--layout` must match the layout `DIR` was built with.

`--under PREFIX` places every source path under `PREFIX/` in the output,
which keeps unrelated drives from colliding:

```
$ granite merge /mnt/old-laptop --into /tank --under 2014-laptop
```

### `granite verify <ROOT>`

Re-hashes every file, ignoring hints, and diffs against the index.

```
$ granite verify /tank/photos
index scanned at 2026-09-09T14:02:11Z
matched 453114
  missing   2014/IMG_0007.CR2
  changed   2014/notes.txt  index 18ff5e4a…  disk 6a9f75ee…
missing 1, unindexed 0, changed 1, unreadable 0
$ echo $?
3
```

### `granite fingerprint <TARGET>...`

```
$ granite fingerprint /mnt/drive-a ~/granite-indexes/DVD_2014.index
4c1a0e7d…  /mnt/drive-a
731081fd…  ~/granite-indexes/DVD_2014.index
```

The fingerprint is the BLAKE3 of the index file. Two shards with identical
content at identical relative paths have identical fingerprints, so "have I
merged this drive already" is a 32-byte comparison.

## Exit codes

| code | meaning |
|---|---|
| 0 | success |
| 1 | usage or argument error |
| 2 | completed, but some entries were skipped or unreadable |
| 3 | verification or hash mismatch |
| 4 | index format version or semantic-key refusal |

## Formats

See [FORMAT.md](FORMAT.md) for the normative description of the index,
hints and meta files, and [DECISIONS.md](DECISIONS.md) for choices made where
the specification was silent.

While the index format version is `0`, an index is only readable by the exact
granite version that wrote it. Re-run `granite scan --force` after upgrading.
