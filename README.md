# granite

**permanent storage**

`granite` is a tool for permanent storage of files across many shards.

An opinionated set of axioms and design decisions make usage extremely straightforward and reliable.

axiom | |
---|---
storage is cheap | different bytes; different file
no deletion | files are permanent
file content is identity | different metadata; different file
storage is reliable | bit rot must be handled at storage layer
idempotent and order independent | `merge(merge(A,B),C) == merge(A,B,C)`
