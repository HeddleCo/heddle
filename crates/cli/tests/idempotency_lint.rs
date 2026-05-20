// SPDX-License-Identifier: Apache-2.0
//! Idempotency-discipline lint.
//!
//! Every state-changing handler under
//! `crates/server/src/server/grpc_hosted_impl/*.rs` must route through
//! the dedup middleware in `idempotency.rs` — either
//! [`with_idempotency`](::weft_server::server::grpc_hosted_impl::idempotency)
//! / [`wrap_idempotent`](::weft_server::server::grpc_hosted_impl::idempotency)
//! for unary RPCs, or `wrap_idempotent_stream` for the streaming pair
//! (`Push` / `Pull`).
//!
//! Why a separate text-based lint rather than runtime discovery? The
//! audit-idempotency CLI tool enumerates the proto's state-changing
//! verbs (every message that carries `client_operation_id = 15`) and
//! asks the live server whether each handler is wired. That works for
//! end-to-end coverage but doesn't catch a regression on a single PR
//! that adds a new handler without dedup. This test runs in `cargo
//! test` and fails the PR locally — same shape as
//! [`crates/cli/tests/op_id_coverage.rs`] but on the server side.
//!
//! Source of truth: the proto. We parse `crates/grpc/proto/heddle/v1/service.proto`
//! and harvest every message that carries a `client_operation_id`
//! field — those are the request types of state-changing RPCs. We
//! then scan the service definitions to recover the RPC method names
//! that take those messages. For each method, the handler under
//! `grpc_hosted_impl/` is checked for one of `with_idempotency`,
//! `wrap_idempotent`, or `wrap_idempotent_stream`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

/// Files under `grpc_hosted_impl/` that don't host a service impl —
/// the lint scans the rest. Skip mod.rs (the GrpcHostedService
/// struct), helper modules, and the dedup middleware itself.
const SKIP_FILES: &[&str] = &[
    "mod.rs",
    "helpers.rs",
    "auth_helpers.rs",
    "content_helpers.rs",
    "events.rs",
    "feed.rs",
    "rate_limit.rs",
    "idempotency.rs",
];

/// State-changing RPCs the lint should not enforce. Each entry is a
/// `snake_case` handler name plus a documented reason. Adding to this
/// list is a deliberate decision — review with scrutiny.
///
/// The user/auth/admin sweep landed in
/// `crates/server/src/server/grpc_hosted_impl/{user,auth,content,sync,workflow}.rs`,
/// so this list is now empty. Add an entry only when a handler is
/// genuinely read-shaped — e.g. it returns the same data on every
/// call and has no side effects beyond audit-logging.
const ALLOWLIST: &[(&str, &str)] = &[];

#[test]
fn every_state_changing_rpc_routes_through_dedup() {
    let proto = workspace_root()
        .join("crates")
        .join("grpc")
        .join("proto")
        .join("heddle")
        .join("v1")
        .join("service.proto");
    let proto_src =
        std::fs::read_to_string(&proto).unwrap_or_else(|e| panic!("read {}: {e}", proto.display()));

    // Step 1: messages that carry `client_operation_id = 15`. Those
    // are the wire shape of every state-changing request.
    let state_changing_messages = harvest_state_changing_messages(&proto_src);
    assert!(
        !state_changing_messages.is_empty(),
        "no messages with `client_operation_id = 15` found in {} — has the \
         dedup convention changed?",
        proto.display()
    );

    // Step 2: rpc methods whose request type is one of those
    // messages. Map of `snake_case_handler_name -> proto_rpc_name`.
    let state_changing_rpcs = harvest_state_changing_rpcs(&proto_src, &state_changing_messages);
    assert!(
        !state_changing_rpcs.is_empty(),
        "no rpcs reference any state-changing message in {} — proto layout \
         broke?",
        proto.display()
    );

    // Step 3: collect handler bodies from grpc_hosted_impl/*.rs.
    let server_impl_dir = workspace_root()
        .join("crates")
        .join("server")
        .join("src")
        .join("server")
        .join("grpc_hosted_impl");
    // The `crates/server` tree lives in the closed-source weft workspace
    // and is not present in the OSS heddle repo. Skip the directory scan
    // (and implicitly pass) when building outside that monorepo.
    if !server_impl_dir.exists() {
        return;
    }
    let mut handlers: BTreeMap<String, (PathBuf, usize, String)> = BTreeMap::new();
    for entry in std::fs::read_dir(&server_impl_dir)
        .unwrap_or_else(|e| panic!("read dir {}: {e}", server_impl_dir.display()))
    {
        let entry = entry.expect("read dir entry");
        let path = entry.path();
        if !path.is_file() || path.extension().map(|e| e != "rs").unwrap_or(true) {
            continue;
        }
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        if SKIP_FILES.contains(&file_name.as_str()) {
            continue;
        }
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        for func in extract_async_fns(&source) {
            handlers
                .entry(func.name.clone())
                .or_insert_with(|| (path.clone(), func.line, func.body.clone()));
        }
    }

    let allowlisted: BTreeSet<&str> = ALLOWLIST.iter().map(|(n, _)| *n).collect();

    let mut missing_handler: Vec<String> = Vec::new();
    let mut unwrapped: Vec<String> = Vec::new();
    for (handler_name, rpc_name) in &state_changing_rpcs {
        if allowlisted.contains(handler_name.as_str()) {
            continue;
        }
        match handlers.get(handler_name) {
            None => {
                missing_handler.push(format!(
                    "rpc `{rpc_name}` (handler `{handler_name}`) — no \
                     `async fn` of that name in grpc_hosted_impl/. Either \
                     the handler hasn't landed yet, or the snake_case \
                     translation in this lint is wrong (override \
                     manually if so)."
                ));
            }
            Some((path, line, body)) => {
                if !body_routes_through_dedup(body) {
                    unwrapped.push(format!(
                        "{}:{} `async fn {}` (rpc `{}`) is state-changing \
                         but its body doesn't call `with_idempotency`, \
                         `wrap_idempotent`, or `wrap_idempotent_stream`",
                        relative_to_workspace(path).display(),
                        line,
                        handler_name,
                        rpc_name,
                    ));
                }
            }
        }
    }

    if !unwrapped.is_empty() {
        panic!(
            "{} hosted RPC handler(s) skip the dedup middleware:\n  - {}\n\n\
             Wire one of `with_idempotency`, `wrap_idempotent`, or \
             `wrap_idempotent_stream` around the body, or — if the \
             handler is genuinely read-shaped — add it to ALLOWLIST in \
             this test with a comment explaining why.",
            unwrapped.len(),
            unwrapped.join("\n  - ")
        );
    }

    // `missing_handler` is informational rather than fatal: the proto
    // adds RPCs ahead of the hosted impls. We log to stderr so the
    // first few lines of the test output show what's pending.
    if !missing_handler.is_empty() {
        eprintln!(
            "Note: {} state-changing rpc(s) have no handler in \
             grpc_hosted_impl/ yet — wiring will be enforced once they \
             land:\n  - {}",
            missing_handler.len(),
            missing_handler.join("\n  - ")
        );
    }
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root from CARGO_MANIFEST_DIR")
        .to_path_buf()
}

fn relative_to_workspace(path: &Path) -> PathBuf {
    let root = workspace_root();
    path.strip_prefix(&root)
        .map(Path::to_path_buf)
        .unwrap_or(path.to_path_buf())
}

fn body_routes_through_dedup(body: &str) -> bool {
    body.contains("wrap_idempotent_stream")
        || body.contains("wrap_idempotent")
        || body.contains("with_idempotency")
}

/// Find every `message Foo { ... string client_operation_id = 15; }`
/// in the proto. Returns the message names (`PascalCase`).
fn harvest_state_changing_messages(proto_src: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let bytes = proto_src.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `message <Name> {`.
        if !looks_like_message_start(bytes, i) {
            i += 1;
            continue;
        }
        let after_kw = i + b"message".len();
        let mut j = after_kw;
        while j < bytes.len() && bytes[j].is_ascii_whitespace() {
            j += 1;
        }
        let name_start = j;
        while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
            j += 1;
        }
        if j == name_start {
            i += 1;
            continue;
        }
        let name = &proto_src[name_start..j];
        // Walk to opening `{`.
        while j < bytes.len() && bytes[j] != b'{' {
            j += 1;
        }
        if j == bytes.len() {
            break;
        }
        let open = j;
        let close = match_curly(bytes, open).unwrap_or(bytes.len() - 1);
        let body = &proto_src[open..=close];
        if body.contains("client_operation_id = 15") {
            out.insert(name.to_string());
        }
        i = close + 1;
    }
    out
}

fn looks_like_message_start(bytes: &[u8], pos: usize) -> bool {
    let kw = b"message";
    if pos + kw.len() >= bytes.len() {
        return false;
    }
    if &bytes[pos..pos + kw.len()] != kw {
        return false;
    }
    if pos > 0 {
        let prev = bytes[pos - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return false;
        }
    }
    let next = bytes[pos + kw.len()];
    next == b' ' || next == b'\t'
}

/// Walk every `service Foo { rpc Bar(Req) returns (...) ; }` block
/// and harvest the `(handler_snake, RpcName)` pairs whose `Req`
/// matches `state_changing`.
fn harvest_state_changing_rpcs(
    proto_src: &str,
    state_changing: &BTreeSet<String>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    // Naive line scan — robust enough for the existing proto.
    for line in proto_src.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with("rpc ") {
            continue;
        }
        // `rpc Method(MaybeStream RequestMsg) returns (stream Resp);`
        let rest = trimmed[b"rpc ".len()..].trim_start();
        let (method, rest) = match rest.split_once('(') {
            Some(pair) => pair,
            None => continue,
        };
        let method = method.trim();
        let req_full = match rest.split_once(')') {
            Some((req, _)) => req.trim(),
            None => continue,
        };
        // Drop a leading `stream` token if present.
        let req = req_full
            .trim()
            .strip_prefix("stream ")
            .map(str::trim)
            .unwrap_or(req_full);
        if state_changing.contains(req) {
            let handler = pascal_to_snake(method);
            out.insert(handler, method.to_string());
        }
    }
    out
}

fn pascal_to_snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[derive(Debug)]
struct AsyncFn {
    name: String,
    line: usize,
    body: String,
}

/// Find every `async fn <name>(...)` whose body is `{ ... }`. Returns
/// the function name, the source line where the `async fn` token
/// starts, and the raw body text (including outer braces).
fn extract_async_fns(source: &str) -> Vec<AsyncFn> {
    let mut out = Vec::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
            continue;
        }
        if bytes[i] == b'"' {
            i = skip_string(bytes, i);
            continue;
        }
        if matches_keyword(bytes, i, b"async") {
            let mut j = i + b"async".len();
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if matches_keyword(bytes, j, b"fn") {
                let mut k = j + b"fn".len();
                while k < bytes.len() && bytes[k].is_ascii_whitespace() {
                    k += 1;
                }
                let name_start = k;
                while k < bytes.len() && (bytes[k].is_ascii_alphanumeric() || bytes[k] == b'_') {
                    k += 1;
                }
                if k == name_start {
                    i += 1;
                    continue;
                }
                let name = source[name_start..k].to_string();
                let body_open = match find_signature_body_open(bytes, k) {
                    Some(pos) => pos,
                    None => {
                        i += 1;
                        continue;
                    }
                };
                let body_close = match_curly(bytes, body_open).unwrap_or(bytes.len() - 1);
                let line = source[..i].bytes().filter(|b| *b == b'\n').count() + 1;
                out.push(AsyncFn {
                    name,
                    line,
                    body: source[body_open..=body_close].to_string(),
                });
                i = body_close + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn matches_keyword(bytes: &[u8], pos: usize, kw: &[u8]) -> bool {
    if pos + kw.len() > bytes.len() {
        return false;
    }
    if &bytes[pos..pos + kw.len()] != kw {
        return false;
    }
    if pos > 0 {
        let prev = bytes[pos - 1];
        if prev.is_ascii_alphanumeric() || prev == b'_' {
            return false;
        }
    }
    if pos + kw.len() < bytes.len() {
        let next = bytes[pos + kw.len()];
        if next.is_ascii_alphanumeric() || next == b'_' {
            return false;
        }
    }
    true
}

fn skip_string(bytes: &[u8], start: usize) -> usize {
    let mut i = start + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn find_signature_body_open(bytes: &[u8], pos: usize) -> Option<usize> {
    let mut paren: i32 = 0;
    let mut angle: i32 = 0;
    let mut bracket: i32 = 0;
    let mut i = pos;
    while i < bytes.len() {
        match bytes[i] {
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'<' => angle += 1,
            b'>' => angle -= 1,
            b'[' => bracket += 1,
            b']' => bracket -= 1,
            b'{' if paren == 0 && bracket == 0 && angle <= 0 => return Some(i),
            b';' if paren == 0 && bracket == 0 => return None,
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'"' => {
                i = skip_string(bytes, i);
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

fn match_curly(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
            b'"' => {
                i = skip_string(bytes, i);
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}