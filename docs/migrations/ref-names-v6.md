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

Loose thread, marker, remote, and synthetic refs, thread records, managed
checkout directories, and timeline recovery/lock paths share the same name
path. Short paths retain reversible UTF-8 percent escaping. Uppercase ASCII
and unsafe bytes escape; the prefix avoids Windows device names. Paths end
in a separate `entry` directory so dropping one name cannot delete another.

The relative name-path budget is **128 bytes**, including separators and
`entry`. Longer encodings use `h-<full 64-hex BLAKE3 digest>/entry` (72 bytes),
hashing the exact native UTF-8 name. The entry's `name` file stores those exact
bytes. Point reads, writes, deletes, and directory enumeration verify the name
against the complete digest path; a mismatched or missing identity is an error.
Digest prefixes never select entries, so names sharing a prefix stay distinct.

The 1024-byte absolute-path guarantee reserves a **512-byte repository root**,
17 bytes for `/.heddle/threads/`, the 128-byte name path, a separator plus a
255-byte managed checkout leaf, and 111 bytes for checkout-local Heddle metadata:
`512 + 17 + 128 + 256 + 111 = 1024`. Remote refs have two bounded name paths
and fit the same allowance. Roots longer than 512 bytes consume that reserved
headroom and remain subject to the host's path limit. This does not require
Windows long-path support for the digest components themselves; Windows's
260-byte absolute-path mode still requires a sufficiently short checkout root.

Imported Git names in the reserved native `heddle/` namespace (case insensitive)
map to `git%` followed by the portable encoded name. Names beginning with
`git%` receive the same mapping so a literal Git name cannot alias an escape.
Native validators reserve `git%` for canonical mappings: a prefixed name is
accepted only when `native_git_name(git_name(name)) == name`. For example,
Git `heddle/foo` becomes native `git%n-heddle%2Ffoo` and Git `git%foo` becomes
native `git%n-git%25foo`. Git export
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

Import discovers raw ref names before decoding. Non-UTF-8 names under
`refs/heads/` or `refs/tags/` are excluded with their exact raw bytes and the
reason "ref name is not valid UTF-8" in the import report. Such names in
ignored namespaces do not block import. Human and JSON output escape invalid
bytes (for example, `\xff`) rather than inventing a replacement-character name.
The commit view, exclusions, and
reflog selection reuse one raw ref snapshot. Imported object reads disable Git
replacement handling because replacement refs are excluded by import policy;
source OIDs continue to identify the original objects.

Weft must mirror `native_git_name` and `git_name` in
`crates/object-model/src/name_encoding.rs`, together with their strict
`encode_name`/`decode_name` helpers. Apply the import mapping before admitting
Git branch/tag identities into native thread/marker and synthetic-frontier
namespaces, and reverse it when projecting them to Git. Preserve signed source
full refs byte-for-byte.
