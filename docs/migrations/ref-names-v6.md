# Repository format v6: exact Git ref names

Format v6 adopts API 0.31.0-alpha.43 and Git branch syntax from Sley 0.11.0.
Branch names retain exact UTF-8 bytes, including `@`, commas, literal U+FFFD,
and trailing non-ASCII whitespace. The signed full-ref limit remains 1024
bytes. Exact `HEAD`, leading `-`, `.lock` components, `..`, `@{`, and Git's
other invalid branch forms are refused. Old native names such as `team:scope`
and `.` are invalid in v6.

## Rebuild a v5 repository

V6 opens only v6. There is no converter, automatic migration, or legacy
storage fallback. Do not change `repository.version` in an existing config.

Before rebuilding, copy uncommitted and untracked files from **every** checkout,
including managed thread checkouts. With the old Heddle version, publish or
export local history you need to retain. Back up the entire `.heddle` directory
and any external object-store directory, together with Git metadata. This
preserves local threads, context, discussions, coordination records, operation
history, and other data that a Git-only re-import does not reconstruct.

Re-clone into a new directory, or initialize a new v6 repository and re-import
its Git history with this Heddle version. Restore saved work files there.
Keep the old repository and backups until you have verified the new history,
all saved files, and required local coordination data. Coordination backups
remain readable with the old version; v6 does not import their old layout.

## Command arguments

Pass names as single quoted arguments when they contain shell punctuation.
Agent fanout's `thread=title` descriptor escapes a name containing `=` as a
bracketed portable encoding: `--lane '[n-feat%2Fmcp%3Dtimeout]=Task'`. The
brackets cannot collide with a Git branch name, and titles can still contain
`=`. Generated fanout commands use this representation automatically.

## Storage and projection

All loose thread, marker, remote, and synthetic refs, thread records, and managed
checkout directories, and timeline recovery/lock paths use one reversible UTF-8 percent encoding. Uppercase ASCII
and unsafe bytes are escaped, and a fixed prefix avoids Windows device names.
Encoding chunks are at most 182 bytes per filesystem component. A separate
`entry` leaf prevents a thread's directory from containing another thread's
checkout; deletion remains independent even for long common prefixes.

Imported Git names in the reserved native `heddle/` namespace (case insensitive)
map to `git%` followed by the portable encoded name. Names beginning with
`git%` receive the same mapping so a literal Git name cannot alias an escape.
For example, Git `heddle/foo` becomes native `git%n-heddle%2Ffoo`. Git export
reverses this mapping. Storage puts canonical imported names beneath a separate
`git` component and escapes the original name once, avoiding repeated expansion
for maximum-length reserved names; synthetic storage likewise encodes the
original owning Git name. Signed source full refs always retain their original
bytes. Synthetic refs remain in their separate typed store.

HEAD and native packed refs remove only LF/CRLF framing. Loose Git input and
Heddle's own HEAD, packed-ref storage, listing and native fetch preserve trailing
U+00A0. Sley 0.11.0 trims Unicode whitespace in Git packed input and its write
validator also refuses trailing Unicode whitespace. Those two Git boundary
cases remain upstream-blocked; Heddle does not bypass Sley. The owner supplied
[sley#243](https://github.com/HeddleCo/sley/issues/243) as the tracking link, but
at verification time that issue describes a pack cursor, so the tracking link
needs correction upstream.
