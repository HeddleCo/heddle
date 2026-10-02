# Local attribution collectors

These adapters emit bounded `AttributionObservation` values through
`heddle integration collect`. Hooks and event streams share that typed boundary;
collection method remains separate from each claim's request/response provenance.
They do not install global hooks, configure telemetry exports, discover saved
transcripts, alter authentication, or capture code automatically.

| Adapter | Accepted version | Methods | Causal join | File coverage |
|---|---|---|---|---|
| Claude Code | 2.1.287 | hook + event_stream | session_id + tool_use_id; response message ID retained | Write, Edit, MultiEdit, NotebookEdit |
| Hermes | 0.21.0 | hook | session_id + turn_id + api_request_id + exact response tool_call_id | Identity-only by default; optional absolute local write_file / patch replace paths |

Unknown versions fail closed at startup. Other versions, Hermes V4A patches,
shell edits, remote/container files, auxiliary calls and generic OTel/proxy
collectors are unsupported. Claude subagent actors come only from hook agent_id;
parent_tool_use_id is not treated as a parent actor. Hermes task_id is not guessed
to be an actor. Its api_request_id can span retries and is never an attempt ID.

## Claude invocation

Use the Heddle build containing `integration collect` and Python 3.11+.
The runner requires POSIX Unix sockets/process groups; only macOS was exercised.
Windows is unsupported. Harness versions are process-observed metadata.

```sh
python3 tools/attribution/claude.py --repo /path/to/project \
  --heddle /path/to/heddle --timeout 120 --prompt 'Your task' -- \
  --model haiku --max-budget-usd 1
```

This launches one print-mode Claude process. The wrapper owns the stream format,
settings and no-session-persistence options. Existing authentication is reused;
no login is started by the adapter. Settings sources are disabled for this
invocation and only temporary PreToolUse/PostToolUse/PostToolUseFailure hooks are
added. Explicit task/tool/permission options may follow `--`; the wrapper never
grants permissions itself. It does not support resumed conversations.

Claude stdout is forwarded unchanged to the caller, **not stored by the adapter**.
It can contain the task's source/text, so redirect it only to a destination you
intend. Only allowlisted identity and structured paths enter Heddle. The socket
is in a fresh mode-0700 temporary directory, mode 0600, removed at exit. No model
map is persisted. The timeout is 1–600 seconds; the child process group receives
TERM then KILL after a three-second grace period. Hook I/O and Heddle subprocesses
have independent deadlines. A collection failure makes the runner exit nonzero.
A forced external SIGKILL can leave an inert temporary directory, but no content
or transcript is written there; it contains hook settings and a socket only.

## Hermes project plugin

The directory containing this README is a Hermes directory plugin (`plugin.yaml`
and `__init__.py`). Place/copy it under a deliberately authorized project's
`.hermes/plugins/heddle-attribution/`. Hermes 0.21.0 project discovery requires
`HERMES_ENABLE_PROJECT_PLUGINS=1` and the applicable plugin allow-list/consent.
This repository does not automatically enable or install it.

Supply these only for that invocation:

```sh
HERMES_ENABLE_PROJECT_PLUGINS=1 \
HEDDLE_ATTRIBUTION_REPO=/absolute/project \
HEDDLE_ATTRIBUTION_BIN=/absolute/heddle \
hermes chat ...
```

The plugin validates process cwd equals the configured project. By default it
records identities only. Set `HEDDLE_ATTRIBUTION_LOCAL_FILES=1` **only** when the
Hermes file tools are configured to operate on this same local filesystem.
Then absolute paths within the project for write_file and patch replace may
bind to content. Relative paths, V4A patches, shell tools and symlink escapes
remain unbound. This option is an operator configuration assertion, not proof
of backend identity. No remote file backend is certified.

Native pre/post_tool_call events provide tool IDs and file boundaries.
post_api_request provides selected model/provider plus response_model and
`response.assistant_message.tool_calls[].id`. Only a full exact tuple joins
these observations. The original model alias and response model stay separate.
Only native `status="ok"` closes a successful tool operation; missing/error/
cancelled status remains unresolved. Raw response, arguments, result, base_url,
headers, prompts, usage and error messages are not forwarded or stored.

The contract was inspected in installed Hermes 0.21.0 (upstream 63279301):
`hermes_cli/__init__.py`, `hermes_cli/plugins.py`,
`agent/conversation_loop.py` post_api_request, `model_tools.py`
_emit_post_tool_call_hook, and `tools/file_tools.py`. Hermes may rewrite duplicate
tool IDs after a response hook; unmatched rewritten IDs remain unknown.
No successful live Hermes inference is claimed; provider access is still pending.

## Ordering, conflicts and limits

Each invocation keeps at most 64 identities and 64 observed operations. Maps do
not evict old keys and accidentally reuse their identity. Two distinct facts
poison a key permanently for that invocation; both reach the journal as
conflicting observations. Additional variants are dropped without restoring
certainty. A duplicate message is idempotent. Different session/request/tool IDs
never borrow a model from each other or from the current session selection.

`observe` enriches an already pending/completed operation without hashing files,
opening a tool operation, or completing it. Late metadata before capture can
therefore be retained. Capture freezes evidence: metadata arriving after a
published operation cannot rewrite history, and unknown identity stays unknown.
A late conflict after capture cannot retroactively invalidate an immutable state.
Capture after the collector exits if complete event-stream coverage is required.

Stream/hook JSON input is capped at 1 MiB; oversize stream frames are forwarded
but not parsed. Identity fields are capped at 256 UTF-8 bytes. Only 64 tool blocks
per assistant event are examined. Normalized stdin is capped at 64 KiB, paths at
32 / 1024 bytes, and Rust's canonical evidence validation is authoritative.
No raw event bags cross the shared collection interface.

## Tests

```sh
python3 -m unittest discover -s tools/attribution -p 'test_*.py'
# Optional real local journal/capture test with synthetic hook fixtures:
HEDDLE_COLLECTOR_TEST_BINARY=/absolute/heddle \
HEDDLE_HOME=/temporary/isolated/home \
python3 -m unittest discover -s tools/attribution -p 'test_*.py'
cargo test --locked -p heddle-verbs --lib operation_attribution
```

Offline fixtures cover both installed contracts, ordering, duplicates, conflicting
models, unknown models, subagent scope, bounds and path rejection. These fixtures
are not represented as native harness emissions. The local validation report
records separately any live Claude smoke and its exact model/version.
