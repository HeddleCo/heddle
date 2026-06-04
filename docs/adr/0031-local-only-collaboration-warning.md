# Local-only collaboration warning

When collaboration records exist locally without a configured or synchronized Heddle remote, Heddle should label that state in surfaces such as `status`, `inbox`, `doctor`, and Git push-adjacent output. The warning appears only when local collaboration exists and should not block readiness unless policy requires hosted sync.

When a `discuss` write creates local-only collaboration without a configured or synchronized Heddle remote, output should include a concise local-only notice and point to Heddle remote sync. Read-only commands can be quieter and rely on orientation surfaces.

Git push-adjacent output should mention Heddle collaboration when local collaboration exists. The message should be specific that Git push does not share Heddle discussions, context, or attention records; Heddle-hosted sync is required.

Hosted rejection or blocked collaboration sync should appear in `status` when it affects the current checkout or thread, but it must be described as a collaboration sync problem rather than dirty source history. `ready` can block through attention severity, while `status` keeps source history and the collaboration sync lane separate.

In the first local discussion slice, `status` should summarize discussions only when they create attention for the current checkout or thread.

<!-- doctor-docs:planned -->
It should point to `heddle inbox` for attention details.

It can also point to `heddle discuss list` for discussion details rather than becoming a discussion list.

**Status:** proposed

**Considered Options:** Staying silent would preserve output restraint, but users in Git-overlay repos could wrongly assume Git push shared discussions or attention. Warning on every command would be noisy, so the label belongs on orientation and sync-adjacent surfaces.
