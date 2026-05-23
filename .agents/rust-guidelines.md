# Rust Development Guidelines

## Code Style

1. **Follow Rust idioms**
   - Use `Result<T, E>` for fallible operations, not panics
   - Prefer `?` operator over `.unwrap()` in library code
   - Use `.expect("reason")` only when panic is truly unrecoverable
   - Prefer iterators over manual loops when readable

2. **Error handling**
   - Define domain-specific error types using `thiserror`
   - Use `anyhow::Result` in CLI/binary code for convenience
   - Use specific error types in library code for composability
   ```rust
   // Good - library code
   #[derive(Debug, thiserror::Error)]
   pub enum StoreError {
       #[error("object not found: {0}")]
       NotFound(String),
       #[error("io error: {0}")]
       Io(#[from] std::io::Error),
   }
   
   // Good - binary/CLI code
   fn main() -> anyhow::Result<()> { ... }
   ```

3. **Naming conventions**
   - Types: `PascalCase`
   - Functions/methods: `snake_case`
   - Constants: `SCREAMING_SNAKE_CASE`
   - Modules: `snake_case`
   - Use descriptive names; avoid abbreviations except well-known ones (id, config, etc.)

4. **Module organization**
   - One primary type per file when the type is complex
   - Use `mod.rs` to re-export public API
   - Keep modules focused and cohesive
   ```rust
   // src/object/mod.rs
   mod blob;
   mod tree;
   mod state;
   
   pub use blob::Blob;
   pub use tree::{Tree, TreeEntry};
   pub use state::State;
   ```

5. **Documentation**
   - Document all public items with `///` doc comments
   - Include examples for non-trivial functions
   - Use `//!` for module-level documentation

6. **Code Size Management**
   - **MAX FILE SIZE**: 300 lines per file
   - **MAX FUNCTION SIZE**: 100 lines per function
   - Extract common patterns into utility modules
   - Use subcrates for large, independent features
   - Prefer composition over large monolithic modules

## Code Quality Checklist

- [ ] No file exceeds 300 lines
- [ ] No function exceeds 100 lines
- [ ] Error handling is consistent (custom types for lib, anyhow for CLI)
- [ ] Common patterns are extracted to utility modules
- [ ] Tests are well-organized and focused
- [ ] Documentation is complete for public APIs
- [ ] Code follows single responsibility principle

## Dependencies

Current dependency stack (see `Cargo.toml`):
- `clap` - CLI parsing with derive macros
- `serde` + `serde_json` + `rmp-serde` - Serialization
- `blake3` - Hashing
- `thiserror` + `anyhow` - Error handling
- `walkdir` - Directory traversal
- `ignore` - Gitignore-style pattern matching
- `chrono` - Timestamps
- `hex` - Hash display
- `tempfile` - Testing
- `tokio` - Async runtime
- `tracing` - Instrumentation

**Adding dependencies:**
- Prefer well-maintained crates with active development
- Check license compatibility (MIT/Apache-2.0 preferred)
- Avoid duplicating functionality already in dependencies

## Security Patterns (Hard-Won)

See `review-pitfalls.md` for the full list with examples. Quick reference:

| Anti-pattern | Safe alternative |
|-------------|-----------------|
| `fs::write(path, data)` for critical files | Write temp → `fsync` → `rename` |
| `path.exists()` then `File::create` | `OpenOptions::new().create_new(true)` |
| `Command::new(x)` without `env_clear()` | Add `.env_clear()` + explicit vars |
| `Result<bool>` for verification functions | `Result<(), Error>` (failure = `Err`) |
| `unwrap_or_default()` in crypto/security | Return `Err(...)` |
| `is_ok()` for feature-flag env vars | Check the value: `"1"`, `"true"`, `"yes"` |
| `WalkBuilder` without `.follow_links(false)` | Always set `.follow_links(false)` |
| `is_dir()` before `is_symlink()` | Check `is_symlink()` first |
| `remove_dir_all` on structured dirs | Walk depth-first + `remove_file`/`remove_dir` |
| Read lock acquired after first read | Acquire write lock before first read in a mutation |

---

## Performance Considerations

1. **Avoid unnecessary allocations**
   - Use `&str` over `String` when ownership not needed
   - Use `Cow<'_, str>` for conditional ownership
   - Prefer `&[u8]` over `Vec<u8>` for read-only byte slices

2. **File I/O**
   - Use buffered readers/writers for sequential access
   - Consider memory-mapping for large files (future optimization)

3. **Hashing**
   - BLAKE3 is fast; don't micro-optimize around it
   - Use incremental hashing for large files
