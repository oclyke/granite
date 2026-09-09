# granite on-disk formats

This document is normative. A reader or writer that follows it interoperates
with granite without reference to granite's source code. Where this document
and the source disagree, this document is the bug report.

Format version described: `granite_index_version: 0` (alpha, see §5).

## 1. Files

Per shard, in a `.granite/` directory at the shard root:

| file | role | committed? |
|---|---|---|
| `.granite/index` | canonical, sorted, deterministic content index | yes |
| `.granite/hints` | rescan-skip cache | no (disposable) |
| `.granite/scan.meta` | provenance of the last scan | no (disposable) |
| `.granite/scan.journal` | present only during, or after a failed, scan | no |
| `.granite/skipped` | log of entries the scan could not index | no (informational) |

With an external index directory (`--index-dir DIR --name NAME`) the same
files are `DIR/NAME.index`, `DIR/NAME.hints`, `DIR/NAME.scan.meta`,
`DIR/NAME.scan.journal`, `DIR/NAME.skipped`. Nothing about any file's
*content* depends on where it is stored.

`NAME` defaults to the shard directory's basename with every byte outside
`[A-Za-z0-9._-]` replaced by `_`, or `shard` if that leaves nothing.

## 2. Common structure: header plus records

Every file except `scan.meta` is a **header** followed by **records**.

The file is a byte sequence, not text. The header region is UTF-8; the record
region need not be.

### 2.1 Header

```
---\n
key: value\n
...
---\n
```

- Line 1 is exactly `---` followed by LF.
- The header body ends at the first subsequent line that is exactly `---`
  followed by LF. Records begin at the next byte.
- The body must be valid UTF-8 and valid YAML 1.2 forming a single flat
  mapping. Values are plain scalars or flow sequences of plain scalars
  (`[a, b]`, `[]`). No anchors, aliases, tags, nested mappings, block
  scalars, multi-document streams, or comments.
- The body is at most 65536 bytes.
- Writers emit keys in the order given for each file type, one per line as
  `key: value` with a single space, sequences as `[a, b]` with `, `
  separators, and never quote scalars. This makes the header bytes a pure
  function of its values.
- Readers must ignore keys they do not recognise unless the key is named in
  `requires` (§5.3).

### 2.2 Records

Each record is one line: fields separated by a single TAB (0x09), terminated
by a single LF (0x0A). No trailing whitespace; the last record ends with LF.
An empty record region (header immediately followed by end of file) is valid.

### 2.3 Sort: `line-bytewise`

Records are sorted by comparing the bytes of each line *excluding* its
terminating LF, ascending, unsigned, shorter-prefix-first. This is the order
`LC_ALL=C sort` produces. Duplicate lines are forbidden. A reader must reject
a file whose records are not strictly ascending.

## 3. Path encoding: `escaped-bytes-v1`

A path is an arbitrary byte sequence. Exactly four bytes are escaped:

| byte | written as |
|---|---|
| 0x5C `\` | `\\` |
| 0x09 TAB | `\t` |
| 0x0A LF | `\n` |
| 0x0D CR | `\r` |

Every other byte, including 0x00–0x08, 0x7F, and all bytes ≥ 0x80, is
written literally. A decoder must reject a backslash followed by anything
other than `\`, `t`, `n`, `r`, and a trailing lone backslash.

Paths are not normalised: NFC and NFD spellings of the same visible name are
different paths. Case is preserved. No transcoding is performed.

An index path is relative to the shard root, uses `/` as separator, has no
leading `/`, no empty component, and no `.` or `..` component. Readers must
reject anything else.

## 4. The index

### 4.1 Header

Keys, in this order:

```
granite_index_version: 0
producer: granite 0.1.0
hash: blake3-256
path_encoding: escaped-bytes-v1
record_fields: [hash, size, path]
sort: line-bytewise
requires: []
```

| key | meaning |
|---|---|
| `granite_index_version` | Integer format version. `0` means alpha (§5.1). |
| `producer` | Tool name and version that wrote the file. Informational, except during alpha. |
| `hash` | Hash function and output length used for identity. Only `blake3-256` is defined. |
| `path_encoding` | Escaping of the path field. Only `escaped-bytes-v1` is defined. |
| `record_fields` | Field names in record order. |
| `sort` | Record ordering. Only `line-bytewise` is defined. |
| `requires` | Extension names a reader must understand to interpret this file (§5.3). |

### 4.2 Records

```
hash TAB size TAB path LF
```

- `hash`: exactly 64 lowercase hexadecimal characters, the BLAKE3 hash
  (default 256-bit output) of the file's bytes.
- `size`: the file's length in bytes as a decimal integer, no sign, no
  leading zeros (`0` itself is valid), no separators. Must fit in 64 bits.
- `path`: encoded per §3.

Invariants a reader must check and a writer must guarantee:

- Records are strictly ascending line-bytewise (§2.3). Since `hash` is fixed
  width and first, the file is hash-major.
- No two records have the same `(hash, path)`.
- No two records have the same `path`. (One path holds one file.)
- All records with the same `hash` have the same `size`.

The same hash at many paths is normal. Zero-length files are ordinary files
and share one hash.

### 4.3 Determinism

The index is a pure function of the set of `(relative path, content)` pairs
in the shard. It contains no timestamps, host names, device identifiers,
absolute paths, scan durations, or ordering artefacts. Two shards holding
identical bytes at identical relative paths must produce byte-identical
index files whatever their location or scan history.

### 4.4 What is not in the index

Symbolic links, directories (including empty ones), devices, sockets, FIFOs,
and files the scanner could not read. These have no content identity. The
`.granite/` directory itself is never indexed.

### 4.5 Fingerprint

```
fingerprint = BLAKE3-256(all bytes of the index file, header included)
```

Rendered as 64 lowercase hex characters. Because the index is deterministic,
the fingerprint identifies a shard's content-and-paths exactly. It covers the
`producer` line, so during alpha it changes across granite versions.

## 5. Versioning

### 5.1 Alpha: version 0

While `granite_index_version` is `0` the format is unstable. A reader must
accept a version-0 file **only if its `producer` value is byte-identical to
the reader's own producer string**. A mismatch is a hard error, never a
warning. This lets the alpha format change freely without a stale index ever
being silently misread; the cost is a forced rescan.

When the format is declared stable it becomes `granite_index_version: 1` and
this rule is deleted.

### 5.2 Reader policy from version 1 on

- Version equal to the reader's own: accept.
- Version greater than the reader's: refuse, naming both versions.
- Version less than the reader's: accept read-only if this document defines a
  reader for that version; otherwise refuse. A file is never upgraded in
  place. Migration is an explicit operation producing a new file.
- Header keys not recognised and not named in `requires`: ignore.

### 5.3 `requires`

A change that a naive reader would *misinterpret* (rather than merely fail to
see) is shipped under a name listed in `requires`. A reader that does not
recognise every name in `requires` must refuse the file regardless of the
version number. Version 0 defines no names, and readers of version 0 must
still perform this check.

### 5.4 Semantic keys

`hash` and `path_encoding` say what the identities and paths *mean*; the
version says how to *parse*. They are checked independently of the version.
Two indexes with different `hash` or `path_encoding` values must never be
merged, compared, or unioned. A reader that understands only `blake3-256` and
`escaped-bytes-v1` refuses anything else.

## 6. `hints`

Disposable. Header keys, in order:

```
granite_hints_version: 0
producer: granite 0.1.0
hash: blake3-256
path_encoding: escaped-bytes-v1
record_fields: [path, size, mtime_nanos, hash]
sort: line-bytewise
```

Records:

```
path TAB size TAB mtime_nanos TAB hash LF
```

`mtime_nanos` is the file's modification time in nanoseconds since the Unix
epoch as a signed decimal integer (negative before 1970). Records are sorted
line-bytewise, so path-major.

Semantics: on a subsequent scan, a file whose path, size, and mtime all match
a hint may reuse the hint's hash without being read. Nothing else may read
this file. A missing, unparseable, or foreign-producer hints file is ignored.

## 7. `scan.meta`

Disposable, informational. A bare YAML mapping with no `---` delimiters and no
records, keys in this order:

```
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

Timestamps are RFC 3339 in UTC with second precision. No command's behaviour
may depend on this file; `verify` may print `started_at`.

## 8. `scan.journal`

Internal to `scan`. Its presence means the last scan did not complete. Header:

```
granite_journal_version: 0
producer: granite 0.1.0
```

Records, unsorted, appended as files are hashed:

```
hash TAB size TAB mtime_nanos TAB path LF
```

The final line may be incomplete (a torn write at the crash point) and is
ignored on resume. A journal from a different producer is discarded.

## 9. `skipped`

Informational. Header:

```
granite_skipped_version: 0
path_encoding: escaped-bytes-v1
record_fields: [path, reason]
```

Records `path TAB reason LF`, one per entry the scan did not index, in walk
order. `reason` is free text (e.g. `symlink`, `empty directory`, an OS error
message).

## 10. Merge output conventions

A merge output directory is itself a shard: it receives a `.granite/index`
describing what was written.

### 10.1 Path collisions (`paths` layout)

A path is a location in the output, and one location holds one file. When
the inputs claim one path for more than one hash, a merge in `paths` layout
writes nothing and reports every such path with the shard and path of each
claimant. The user resolves it by renaming claimants in their source shards
and rescanning those shards. No automatic renaming is performed, so the
output tree contains exactly the paths that appear in the inputs.

### 10.2 `hash` layout

Content is written once per hash at `objects/<first two hex chars>/<hash>`.
The union's `(hash, size, path)` records are written, in index format, to
`.granite/paths`. `.granite/index` describes the `objects/` tree.
