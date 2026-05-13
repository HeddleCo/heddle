// Common abstract types for Heddle VCS formal specifications.
//
// Values are realistic Heddle identifiers and paths to make traces
// readable. The model checker only cares about distinctness, but
// realistic names make violations easier to diagnose.

module common {
  // Heddle change IDs (128-bit, base32-encoded with hd- prefix)
  pure val CHANGE_IDS = Set(
    "hd-x7k9qm4htv2wn8",
    "hd-a3bf6r2pyw9kmjt",
    "hd-d8nz5v4qec7xh6s",
    "hd-f2jw8m6tab3yrpc",
    "hd-g9ht4k7nxv5qzrd"
  )

  // BLAKE3 content hashes (hex-encoded, truncated for readability)
  pure val CONTENT_HASHES = Set(
    "b3-9a4f2e71c8d0",
    "b3-3c7b8d15fe92",
    "b3-e6a1f4d82b30",
    "b3-7d5c0e93a4f1",
    "b3-1b8f6a2d7e40"
  )

  // File paths reflecting a real Rust project
  pure val FILE_PATHS = Set(
    "src/store/repository.rs",
    "src/refs/refs_manager.rs",
    "src/cli/commands/merge.rs",
    "tests/integration.rs"
  )

  // Thread (branch) names
  pure val THREAD_NAMES = Set("main", "feat/auth-flow", "agent/agent-abc123")

  // Marker (tag) names
  pure val MARKER_NAMES = Set("v0.1.0", "v0.2.0")

  // Agent session IDs (base32-encoded, agent- prefix)
  pure val SESSION_IDS = Set("agent-n4kw7xqm3v", "agent-h8tb2ypf6r", "agent-c5jz9dma4e")

  // Process IDs for lock contention modeling
  pure val PROCESS_IDS = Set("main-cli", "agent-worker-1", "agent-worker-2")

  // Worktree IDs (base32-encoded, wt- prefix)
  pure val WORKTREE_IDS = Set("wt-r3kf8m2qvx", "wt-p7nt5b9djw", "wt-y6ha4c1gzs")
}
