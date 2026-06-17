import { test } from "node:test";
import assert from "node:assert/strict";
import { Heddle, HeddleError, HeddleStreamingVerbError, ExecStreamError, type Executor, type ExecRequest, type ExecResult } from "../src/index.js";
import type {
  StatusSchema,
  TimelineForkSchema,
  TimelineLogSchema,
  WatchLineSchema,
} from "../generated/heddle-schemas.js";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";

/** A deterministic in-memory executor — proves the parsing/error seam works
 *  without building or spawning the real binary. The real-binary smoke test
 *  is gated below on HEDDLE_BIN and the npm CI matrix lands in #584. */
class FakeExecutor implements Executor {
  lastRequest: ExecRequest | undefined;
  constructor(private readonly canned: ExecResult) {}
  exec(req: ExecRequest): Promise<ExecResult> {
    this.lastRequest = req;
    return Promise.resolve(this.canned);
  }
}

const STATUS_JSON: StatusSchema = {
  branch: "main",
  output_kind: "status",
} as unknown as StatusSchema;

const TIMELINE_LOG_JSON: TimelineLogSchema = {
  output_kind: "timeline_log",
  status: "completed",
  repository_capability: "git-overlay",
  storage_model: "git+heddle-sidecar",
  thread: "main",
  cursor: {
    branch_id: "tlb-main",
    step_id: "tls-one",
    state: "hd-state",
    state_full: "hd-state-full",
  },
  branches: [],
  steps: [],
  active_branch_path: ["tlb-main"],
  actions: {
    can_undo: false,
    can_redo: false,
  },
  recovery: null,
};

const TIMELINE_ACTION_JSON: TimelineForkSchema = {
  output_kind: "timeline_action",
  status: "completed",
  action: "fork",
  thread: "main",
  branch_id: "tlb-child",
  parent_branch_id: "tlb-main",
  from_step_id: "tls-one",
  cursor_branch_id: "tlb-main",
  cursor_step_id: "tls-one",
  operation_id: "hto-op",
  recovered_operation_id: null,
  materialized: null,
  materialization_status: null,
  recovery_status: null,
  blocker_count: 0,
  branch_count: 2,
  step_count: 1,
};

test("parses a success payload into the typed shape", async () => {
  const fake = new FakeExecutor({ exitCode: 0, stdout: JSON.stringify(STATUS_JSON), stderr: "" });
  const heddle = new Heddle({ executor: fake });
  const status = await heddle.status();
  assert.equal((status as StatusSchema).output_kind, "status");
  assert.equal(fake.lastRequest?.verb, "status");
  // The executor receives the canonical verb; SpawnExecutor injects
  // `--output json`. Here we just assert the seam carried the verb through.
  assert.deepEqual(fake.lastRequest?.args, []);
});

test("threads --op-id through the executor for mutating verbs", async () => {
  const fake = new FakeExecutor({ exitCode: 0, stdout: "{}", stderr: "" });
  const heddle = new Heddle({ executor: fake });
  await heddle.commit(["-m", "msg"], { opId: "op-123" });
  assert.equal(fake.lastRequest?.opId, "op-123");
  assert.deepEqual(fake.lastRequest?.args, ["-m", "msg"]);
});

test("applies the instance default repoPath through a custom executor", async () => {
  const fake = new FakeExecutor({ exitCode: 0, stdout: JSON.stringify(STATUS_JSON), stderr: "" });
  const heddle = new Heddle({ executor: fake, repoPath: "/repos/demo" });
  await heddle.status();
  assert.equal(fake.lastRequest?.repoPath, "/repos/demo");
  // A per-call repoPath still overrides the instance default.
  await heddle.status([], { repoPath: "/repos/other" });
  assert.equal(fake.lastRequest?.repoPath, "/repos/other");
});

test("maps a non-zero exit to a HeddleError with parsed envelope", async () => {
  const envelope = {
    code: "no_remote",
    error: "no default remote configured",
    exit_code: 78,
    hint: "set a remote",
    kind: "no_remote",
  };
  const fake = new FakeExecutor({ exitCode: 78, stdout: "", stderr: JSON.stringify(envelope) });
  const heddle = new Heddle({ executor: fake });
  await assert.rejects(
    () => heddle.push(),
    (err: unknown) => {
      assert.ok(err instanceof HeddleError);
      assert.equal(err.exitCode, 78);
      assert.equal(err.code, "no_remote");
      assert.equal(err.retryable, false);
      assert.equal(err.message, "no default remote configured");
      return true;
    },
  );
});

test("flags exit 75 (TempFail) as retryable", async () => {
  const fake = new FakeExecutor({
    exitCode: 75,
    stdout: "",
    stderr: JSON.stringify({ code: "transient", error: "try again", exit_code: 75, hint: "", kind: "transient" }),
  });
  const heddle = new Heddle({ executor: fake });
  await assert.rejects(
    () => heddle.fetch(),
    (err: unknown) => {
      assert.ok(err instanceof HeddleError);
      assert.equal(err.retryable, true);
      return true;
    },
  );
});

test("run() refuses a streaming (JSONL) verb instead of mis-parsing it", async () => {
  // `watch` emits JSONL — multiple objects, one per line. A single
  // JSON.parse of this would throw or silently keep only the first object.
  const jsonl = [
    JSON.stringify({ id: 1, kind: "capture", ts: "t1" }),
    JSON.stringify({ id: 2, kind: "commit", ts: "t2" }),
  ].join("\n");
  const fake = new FakeExecutor({ exitCode: 0, stdout: jsonl, stderr: "" });
  const heddle = new Heddle({ executor: fake });
  await assert.rejects(
    // A typed caller is blocked at compile time; the cast exercises the
    // runtime guard that protects untyped JS callers.
    () => (heddle.run as (verb: string) => Promise<unknown>)("watch"),
    (err: unknown) => {
      assert.ok(err instanceof HeddleStreamingVerbError);
      assert.equal(err.verb, "watch");
      // The executor must not even be invoked for a streaming verb on run().
      assert.equal(fake.lastRequest, undefined);
      return true;
    },
  );
});

test("stream() parses a JSONL verb line by line", async () => {
  const lines: WatchLineSchema[] = [
    { id: 1, kind: "capture", ts: "t1" },
    { id: 2, kind: "commit", ts: "t2" },
  ];
  const fake = new FakeExecutor({
    exitCode: 0,
    stdout: lines.map((l) => JSON.stringify(l)).join("\n") + "\n",
    stderr: "",
  });
  const heddle = new Heddle({ executor: fake });
  const collected: WatchLineSchema[] = [];
  for await (const line of heddle.watch()) collected.push(line);
  assert.deepEqual(collected, lines);
  assert.equal(fake.lastRequest?.verb, "watch");
});

test("stream() maps a non-zero exit to a HeddleError", async () => {
  const fake = new FakeExecutor({
    exitCode: 74,
    stdout: "",
    stderr: JSON.stringify({ code: "io", error: "stream failed", exit_code: 74, hint: "", kind: "io" }),
  });
  const heddle = new Heddle({ executor: fake });
  await assert.rejects(
    async () => {
      for await (const _ of heddle.watch()) void _;
    },
    (err: unknown) => {
      assert.ok(err instanceof HeddleError);
      assert.equal(err.exitCode, 74);
      return true;
    },
  );
});

test("stream() yields lines incrementally before the process exits", async () => {
  // A streaming executor that emits the first line, then BLOCKS — modelling
  // a still-running `watch` / `status --watch` that has not exited. If
  // stream() awaited process exit (the old P1 bug) the first `next()` would
  // never resolve. We assert line 1 arrives while the source is still open,
  // then release it for the rest.
  let releaseSecond!: () => void;
  const secondGate = new Promise<void>((resolve) => {
    releaseSecond = resolve;
  });
  class StreamingExecutor implements Executor {
    lastRequest: ExecRequest | undefined;
    exec(): Promise<ExecResult> {
      return Promise.reject(new Error("streaming verb must use execStream"));
    }
    async *execStream(req: ExecRequest): AsyncGenerator<string> {
      this.lastRequest = req;
      yield JSON.stringify({ id: 1, kind: "capture", ts: "t1" });
      await secondGate; // process stays open — no exit yet
      yield JSON.stringify({ id: 2, kind: "commit", ts: "t2" });
    }
  }
  const fake = new StreamingExecutor();
  const heddle = new Heddle({ executor: fake });
  const iter = heddle.watch()[Symbol.asyncIterator]();

  const first = await iter.next();
  assert.equal(first.done, false);
  assert.equal((first.value as WatchLineSchema).id, 1);

  releaseSecond();
  const second = await iter.next();
  assert.equal((second.value as WatchLineSchema).id, 2);

  const end = await iter.next();
  assert.equal(end.done, true);
  assert.equal(fake.lastRequest?.verb, "watch");
});

test("stream() over a watch-mode verb maps a non-zero exit via ExecStreamError", async () => {
  class FailingStreamExecutor implements Executor {
    exec(): Promise<ExecResult> {
      return Promise.reject(new Error("should use execStream"));
    }
    async *execStream(): AsyncGenerator<string> {
      yield JSON.stringify({ output_kind: "status" });
      throw new ExecStreamError({
        exitCode: 74,
        stdout: "",
        stderr: JSON.stringify({ code: "io", error: "watch failed", exit_code: 74, hint: "", kind: "io" }),
      });
    }
  }
  const heddle = new Heddle({ executor: new FailingStreamExecutor() });
  await assert.rejects(
    async () => {
      for await (const _ of heddle.stream("status", ["--watch"])) void _;
    },
    (err: unknown) => {
      assert.ok(err instanceof HeddleError);
      assert.equal(err.exitCode, 74);
      assert.equal(err.code, "io");
      return true;
    },
  );
});

test("timeline() drives the timeline log schema verb", async () => {
  const fake = new FakeExecutor({
    exitCode: 0,
    stdout: JSON.stringify(TIMELINE_LOG_JSON),
    stderr: "",
  });
  const heddle = new Heddle({ executor: fake });

  const timeline = await heddle.timeline({ thread: "main" });

  assert.equal(timeline.output_kind, "timeline_log");
  assert.equal(fake.lastRequest?.verb, "log --timeline");
  assert.deepEqual(fake.lastRequest?.args, ["--thread", "main"]);
});

test("timelineFork() builds an OpenCode native tool-call selector", async () => {
  const fake = new FakeExecutor({
    exitCode: 0,
    stdout: JSON.stringify(TIMELINE_ACTION_JSON),
    stderr: "",
  });
  const heddle = new Heddle({ executor: fake });

  const forked = await heddle.timelineFork(
    {
      kind: "tool-call",
      thread: "main",
      toolCallId: "call_123",
      sessionId: "ses_456",
      messageId: "msg_789",
    },
    { branch: "tlb-child", reason: "fan-out" },
    { opId: "op-fork" },
  );

  assert.equal(forked.output_kind, "timeline_action");
  assert.equal(fake.lastRequest?.verb, "timeline fork");
  assert.equal(fake.lastRequest?.opId, "op-fork");
  assert.deepEqual(fake.lastRequest?.args, [
    "--thread", "main",
    "--tool-call", "call_123",
    "--harness", "opencode",
    "--session", "ses_456",
    "--message", "msg_789",
    "--branch", "tlb-child",
    "--reason", "fan-out",
  ]);
});

test("timelineReset() supports step targets and materialization options", async () => {
  const fake = new FakeExecutor({
    exitCode: 0,
    stdout: JSON.stringify({ ...TIMELINE_ACTION_JSON, action: "reset" }),
    stderr: "",
  });
  const heddle = new Heddle({ executor: fake });

  await heddle.timelineReset(
    {
      kind: "step",
      thread: "main",
      fromBranch: "tlb-main",
      stepId: "tls-one",
    },
    { materialize: true, mode: "fail-if-dirty" },
    { opId: "op-reset" },
  );

  assert.equal(fake.lastRequest?.verb, "timeline reset");
  assert.equal(fake.lastRequest?.opId, "op-reset");
  assert.deepEqual(fake.lastRequest?.args, [
    "--thread", "main",
    "--from-branch", "tlb-main",
    "--step", "tls-one",
    "--materialize",
    "--mode", "fail-if-dirty",
  ]);
});

test("timelineRecover() targets a thread", async () => {
  const fake = new FakeExecutor({
    exitCode: 0,
    stdout: JSON.stringify({ ...TIMELINE_ACTION_JSON, action: "recover" }),
    stderr: "",
  });
  const heddle = new Heddle({ executor: fake });

  await heddle.timelineRecover({ thread: "main" }, { opId: "op-recover" });

  assert.equal(fake.lastRequest?.verb, "timeline recover");
  assert.equal(fake.lastRequest?.opId, "op-recover");
  assert.deepEqual(fake.lastRequest?.args, ["--thread", "main"]);
});

test("run() rejects every jsonl-capable verb", async () => {
  const heddle = new Heddle({
    executor: new FakeExecutor({ exitCode: 0, stdout: "{}", stderr: "" }),
  });
  const call = heddle.run as (verb: string, args?: readonly string[]) => Promise<unknown>;

  // Always-streaming verbs (json_kind "jsonl") are refused unconditionally.
  for (const verb of ["watch"]) {
    await assert.rejects(() => call(verb), (err: unknown) => {
      assert.ok(err instanceof HeddleStreamingVerbError);
      assert.equal(err.verb, verb);
      return true;
    });
  }

  // Watch-mode verbs (json_kind "json_or_jsonl") are refused ONLY when a
  // watch flag flips them into a stream.
  for (const verb of ["status", "thread show"]) {
    await assert.rejects(() => call(verb, ["--watch"]), (err: unknown) => {
      assert.ok(err instanceof HeddleStreamingVerbError);
      assert.equal(err.verb, verb);
      return true;
    });
  }
});

test("run() still accepts a watch-mode verb in single-payload mode", async () => {
  const fake = new FakeExecutor({ exitCode: 0, stdout: JSON.stringify(STATUS_JSON), stderr: "" });
  const heddle = new Heddle({ executor: fake });
  // No --watch → a single JSON payload, parsed normally.
  const status = await heddle.run("status");
  assert.equal((status as StatusSchema).output_kind, "status");
  assert.equal(fake.lastRequest?.verb, "status");
});

test("real-binary smoke test", { skip: !process.env["HEDDLE_BIN"] }, async () => {
  const heddle = new Heddle({ binaryPath: process.env["HEDDLE_BIN"] });
  // `schemas` is read-only and works outside a repo: a stable known verb.
  const schemas = await heddle.run("schemas");
  assert.ok(schemas, "schemas payload should parse");
});

test("real-binary error envelope maps status outside a repo", { skip: !process.env["HEDDLE_BIN"] }, async () => {
  const nonRepo = await mkdtemp(join(tmpdir(), "heddle-npm-client-"));
  try {
    const heddle = new Heddle({
      binaryPath: process.env["HEDDLE_BIN"],
      cwd: nonRepo,
    });
    await assert.rejects(
      () => heddle.status(),
      (err: unknown) => {
        assert.ok(err instanceof HeddleError);
        assert.equal(err.code, "repository_not_found");
        return true;
      },
    );
  } finally {
    await rm(nonRepo, { recursive: true, force: true });
  }
});

// --- SpawnExecutor regression tests (codex catch-up findings, 2026-06-12) ---

import { SpawnExecutor } from "../src/index.js";
import { join } from "node:path";

const ECHO_ARGV = join(process.cwd(), "test", "fixtures", "echo-argv.cjs");
const STREAM_FOREVER = join(process.cwd(), "test", "fixtures", "stream-forever.cjs");
const GENERATE_OUTPUT = join(process.cwd(), "test", "fixtures", "generate-output.cjs");

test("buildArgv puts global flags before verb tokens (value-taking verb flags)", async () => {
  // Regression: `thread marker delete --prefix` ends in a value-taking flag; with
  // globals appended after the verb, `--output` was consumed as the
  // --prefix value and the verb could not run through the API at all.
  const exec = new SpawnExecutor({ binaryPath: ECHO_ARGV });
  const result = await exec.exec({
    verb: "thread marker delete --prefix",
    args: ["foo"],
    repoPath: "/repos/demo",
    opId: "op-9",
  });
  assert.equal(result.exitCode, 0);
  assert.deepEqual(JSON.parse(result.stdout.trim()), [
    "--output", "json",
    "-C", "/repos/demo",
    "--op-id", "op-9",
    "thread", "marker", "delete", "--prefix",
    "foo",
  ]);
});

test("missing heddle binary rejects as a typed HeddleError", async () => {
  const heddle = new Heddle({ binaryPath: "heddle-definitely-not-on-path-for-test" });
  await assert.rejects(
    () => heddle.status(),
    (err: unknown) => {
      assert.ok(err instanceof HeddleError);
      assert.equal(err.exitCode, 74);
      assert.equal(err.code, "binary_not_found");
      assert.equal(err.retryable, false);
      return true;
    },
  );
});

test("SpawnExecutor rejects when buffered output exceeds the configured cap", async () => {
  const exec = new SpawnExecutor({ binaryPath: GENERATE_OUTPUT, maxOutputBytes: 8 });
  await assert.rejects(
    () => exec.exec({ verb: "status", args: ["16"] }),
    (err: unknown) => {
      assert.ok(err instanceof HeddleError);
      assert.equal(err.exitCode, 74);
      assert.equal(err.code, "output_too_large");
      return true;
    },
  );
});

test("execStream kills the child when the consumer stops early", async () => {
  // Regression: breaking out of the async generator after the first event
  // left watch-mode children running until process exit.
  const exec = new SpawnExecutor({ binaryPath: STREAM_FOREVER });
  let childPid: number | undefined;
  for await (const line of exec.execStream({ verb: "watch", args: [] })) {
    childPid = Number(line);
    break; // closes the generator at the yield
  }
  assert.ok(childPid && childPid > 0, "first line is the stub's pid");
  // The finally block sends SIGTERM; give it a beat, then probe liveness.
  const alive = (pid: number) => {
    try { process.kill(pid, 0); return true; } catch { return false; }
  };
  for (let i = 0; i < 40 && alive(childPid); i++) {
    await new Promise((r) => setTimeout(r, 25));
  }
  assert.equal(alive(childPid), false, "stream child must be terminated on early close");
});
