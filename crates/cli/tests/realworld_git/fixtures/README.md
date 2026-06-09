# Real-World Git Fixtures

This directory holds shallow bare clones of four public Git repositories,
captured as `*.tar.gz` and pinned by tip OID in
[`../realworld_repos.toml`](../realworld_repos.toml).

| Fixture | Source | Depth | Shape |
|---|---|---|---|
| `gitoxide-shaped.tar.gz` | GitoxideLabs/gitoxide @ main | 50 | packed-refs, binary-churn, tags |
| `tokio-shaped.tar.gz` | tokio-rs/tokio @ master | 100 | merge-heavy, multi-branch |
| `ripgrep-shaped.tar.gz` | BurntSushi/ripgrep @ master | 200 | many-small-files |
| `git-shaped.tar.gz` | git/git @ master | 10 | deep-dag, octopus, gitlink |

Total committed size: ~24 MB. Refresh with `./vendor.sh`; paste the printed
rewritten tip OIDs into `realworld_repos.toml` before committing.

Each fixture is a `git fast-export | git fast-import` re-rooting of a shallow
upstream clone, so the recorded `commit` is a *post-rewrite* OID, not the
upstream's tip — this is what makes the bare repo self-contained (no missing
boundary commits, no missing partial-clone blobs). `git-shaped` deliberately
keeps its `sha1collisiondetection` gitlink intact: that submodule entry's
commit OID lives in a foreign repository and would have tripped heddle's
clone-time reachability walk, but `git_core::collect_reachable_object_ids`
now skips gitlink entries during the walk so the parent repo round-trips
cleanly.

The `extract_fixture` helper in `cli_integration::realworld_git` untars the
matching tarball into a `TempDir` and returns the path to the bare repo. Each
test that touches one asserts the tip matches the registered `commit`, so a
re-vendored tarball that doesn't update the registry fails fast rather than
silently testing against drifted state.
