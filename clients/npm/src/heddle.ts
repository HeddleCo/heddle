import type {
  HeddleSchemaVerb,
  HeddleVerbOutputs,
  ErrorEnvelopeSchema,
} from "../generated/heddle-schemas.js";
import {
  SpawnExecutor,
  ExecStreamError,
  type Executor,
  type ExecResult,
} from "./executor.js";
import { HeddleError, HeddleStreamingVerbError } from "./errors.js";

/**
 * Verbs that ALWAYS emit JSONL — one JSON object per line — instead of a
 * single JSON payload. These carry `json_kind: "jsonl"` in the CLI command
 * catalog (`heddle watch` streams live oplog activity; `harness-bridge`
 * relays an event stream). A single `JSON.parse` of their stdout is always
 * wrong, so {@link Heddle.run} refuses them outright — callers iterate
 * {@link Heddle.stream}.
 */
export type HeddleStreamingVerb = Extract<
  HeddleSchemaVerb,
  "watch" | "harness-bridge"
>;

/** Runtime mirror of {@link HeddleStreamingVerb} for value-level checks. */
export const HEDDLE_STREAMING_VERBS: readonly HeddleStreamingVerb[] = [
  "watch",
  "harness-bridge",
];

/**
 * Verbs that emit a single JSON payload by default but switch to a JSONL
 * stream under a `--watch`-style flag. These carry `json_kind:
 * "json_or_jsonl"` in the CLI command catalog (`status`, `thread show`,
 * `workspace show` all gain a `--watch` live-refresh mode that prints one
 * JSON snapshot per tick). {@link Heddle.run} accepts them in their default
 * single-payload mode but refuses them when a watch flag is present, since
 * the output then streams; {@link Heddle.stream} iterates the snapshots.
 */
export type HeddleWatchModeVerb = Extract<
  HeddleSchemaVerb,
  "status" | "thread show" | "workspace show"
>;

/** Runtime mirror of {@link HeddleWatchModeVerb} for value-level checks. */
export const HEDDLE_WATCH_MODE_VERBS: readonly HeddleWatchModeVerb[] = [
  "status",
  "thread show",
  "workspace show",
];

/** Flags that flip a {@link HeddleWatchModeVerb} into a JSONL stream. */
export const HEDDLE_WATCH_FLAGS: readonly string[] = ["--watch"];

/** Every JSONL-capable verb: always-streaming plus watch-mode streaming. */
export type HeddleJsonlVerb = HeddleStreamingVerb | HeddleWatchModeVerb;

/** Any schema-backed verb that returns a single JSON payload via `run()`. */
export type HeddleRunVerb = Exclude<HeddleSchemaVerb, HeddleStreamingVerb>;

function isStreamingVerb(verb: string): verb is HeddleStreamingVerb {
  return (HEDDLE_STREAMING_VERBS as readonly string[]).includes(verb);
}

function isWatchModeVerb(verb: string): verb is HeddleWatchModeVerb {
  return (HEDDLE_WATCH_MODE_VERBS as readonly string[]).includes(verb);
}

function requestsWatchMode(args: readonly string[]): boolean {
  return args.some((arg) => HEDDLE_WATCH_FLAGS.includes(arg));
}

export interface HeddleOptions {
  /** Path to the heddle binary. Defaults to "heddle" on PATH. Ignored if
   *  a custom `executor` is supplied. */
  binaryPath?: string | undefined;
  /** Default repo path (`-C`) for every call; overridable per-call. */
  repoPath?: string | undefined;
  /** Working directory for spawned processes. */
  cwd?: string | undefined;
  /** Extra environment for spawned processes. */
  env?: Record<string, string> | undefined;
  /**
   * Override the transport. Defaults to a {@link SpawnExecutor}. Supply a
   * custom {@link Executor} (e.g. a napi/daemon backend, #586) to change
   * how invocations are dispatched without touching call sites.
   */
  executor?: Executor;
}

/** Per-call options applied on top of the instance defaults. */
export interface RunOptions {
  /** `--op-id` for idempotent retries. Honored by mutating verbs. */
  opId?: string | undefined;
  /** Repo path (`-C`) for this call, overriding the instance default. */
  repoPath?: string | undefined;
  /** Cancel an in-flight invocation. */
  signal?: AbortSignal | undefined;
}

/**
 * Transport-agnostic TypeScript API over the heddle CLI's JSON contract.
 *
 * Each call drives `heddle <verb> --output json [...]` through an
 * {@link Executor}, parses the stdout envelope, and returns the
 * `HeddleVerbOutputs`-typed payload. Non-zero exits become a
 * {@link HeddleError} carrying the parsed error envelope and a `retryable`
 * flag (true only for sysexits 75 / TempFail).
 */
export class Heddle {
  private readonly executor: Executor;
  /** Instance default repo path (`-C`), applied regardless of transport. */
  private readonly repoPath: string | undefined;

  constructor(options: HeddleOptions = {}) {
    this.repoPath = options.repoPath;
    if (options.executor) {
      this.executor = options.executor;
    } else {
      this.executor = new SpawnExecutor({
        binaryPath: options.binaryPath,
        repoPath: options.repoPath,
        cwd: options.cwd,
        env: options.env,
      });
    }
  }

  /**
   * Run a single-payload schema-backed verb and return its typed payload.
   * Throws {@link HeddleError} on a non-zero exit or unparseable stdout, and
   * {@link HeddleStreamingVerbError} if given a JSONL/streaming verb — both
   * the always-streaming verbs (the type already excludes them; the runtime
   * guard catches untyped JS callers) and a {@link HeddleWatchModeVerb}
   * invoked with a `--watch`-style flag, which makes its output stream. Use
   * {@link Heddle.stream} for those.
   */
  async run<V extends HeddleRunVerb>(
    verb: V,
    args: readonly string[] = [],
    options: RunOptions = {},
  ): Promise<HeddleVerbOutputs[V]> {
    if (isStreamingVerb(verb)) {
      throw new HeddleStreamingVerbError(verb);
    }
    if (isWatchModeVerb(verb) && requestsWatchMode(args)) {
      throw new HeddleStreamingVerbError(verb);
    }

    const result = await this.executor.exec({
      verb,
      args,
      opId: options.opId,
      repoPath: options.repoPath ?? this.repoPath,
      signal: options.signal,
    });

    if (result.exitCode !== 0) {
      throw new HeddleError({
        verb,
        exitCode: result.exitCode,
        stdout: result.stdout,
        stderr: result.stderr,
        envelope: parseErrorEnvelope(result),
      });
    }

    return parsePayload<V>(verb, result);
  }

  /**
   * Drive a JSONL verb and yield each line's typed payload AS IT ARRIVES.
   * Streaming verbs like `heddle watch` or `status --watch` run
   * indefinitely, so this must not wait for the process to exit before
   * producing values. When the {@link Executor} supports streaming
   * ({@link Executor.execStream}, as the default {@link SpawnExecutor}
   * does), each stdout line is parsed and yielded the moment it lands; the
   * iterator ends when the child's stdout closes. A buffered-only executor
   * (e.g. a daemon backend that implements just {@link Executor.exec}) falls
   * back to splitting the collected output once the verb terminates. Throws
   * {@link HeddleError} on a non-zero exit or an unparseable line.
   */
  async *stream<V extends HeddleJsonlVerb>(
    verb: V,
    args: readonly string[] = [],
    options: RunOptions = {},
  ): AsyncGenerator<HeddleVerbOutputs[V], void, unknown> {
    const req = {
      verb,
      args,
      opId: options.opId,
      repoPath: options.repoPath ?? this.repoPath,
      signal: options.signal,
    };

    if (this.executor.execStream) {
      try {
        for await (const line of this.executor.execStream(req)) {
          const trimmed = line.trim();
          if (trimmed.length === 0) continue;
          yield parseStreamLine<V>(verb, trimmed);
        }
      } catch (err) {
        if (err instanceof ExecStreamError) {
          throw new HeddleError({
            verb,
            exitCode: err.result.exitCode,
            stdout: err.result.stdout,
            stderr: err.result.stderr,
            envelope: parseErrorEnvelope(err.result),
          });
        }
        throw err;
      }
      return;
    }

    const result = await this.executor.exec(req);

    if (result.exitCode !== 0) {
      throw new HeddleError({
        verb,
        exitCode: result.exitCode,
        stdout: result.stdout,
        stderr: result.stderr,
        envelope: parseErrorEnvelope(result),
      });
    }

    for (const line of result.stdout.split("\n")) {
      const trimmed = line.trim();
      if (trimmed.length === 0) continue;
      yield parseStreamLine<V>(verb, trimmed);
    }
  }

  // ---- Harness ops (documented schemas, #581) ----------------------------

  /** `heddle adopt` — bring an existing repo under heddle. Mutating. */
  adopt(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("adopt", args, options);
  }

  /** `heddle init` — initialize a new heddle repo. Mutating. */
  init(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("init", args, options);
  }

  /** `heddle status` — working-state snapshot. Read-only. */
  status(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("status", args, options);
  }

  /** `heddle start` — start a thread/attempt. Mutating. */
  start(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("start", args, options);
  }

  /** `heddle commit` — capture working changes. Mutating. */
  commit(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("commit", args, options);
  }

  /** `heddle log` — history. Read-only. */
  log(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("log", args, options);
  }

  /** `heddle diff` — working/range diff. Read-only. */
  diff(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("diff", args, options);
  }

  /** `heddle fetch` — fetch objects/refs from a remote. Mutating. */
  fetch(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("fetch", args, options);
  }

  /** `heddle push` — push to a remote. Mutating. */
  push(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("push", args, options);
  }

  /** `heddle bridge git export` — export to a git repo. Mutating. */
  export(args: readonly string[] = [], options: RunOptions = {}) {
    return this.run("bridge git export", args, options);
  }

  /** `heddle watch` — stream live oplog activity (JSONL). Read-only. */
  watch(args: readonly string[] = [], options: RunOptions = {}) {
    return this.stream("watch", args, options);
  }
}

/** Parse one JSONL line, wrapping a JSON error as a HeddleError. */
function parseStreamLine<V extends HeddleSchemaVerb>(
  verb: V,
  trimmed: string,
): HeddleVerbOutputs[V] {
  try {
    return JSON.parse(trimmed) as HeddleVerbOutputs[V];
  } catch {
    throw new HeddleError({
      verb,
      exitCode: 0,
      stdout: trimmed,
      stderr: "",
    });
  }
}

/** Parse a success payload, wrapping JSON errors as a HeddleError. */
function parsePayload<V extends HeddleSchemaVerb>(
  verb: V,
  result: ExecResult,
): HeddleVerbOutputs[V] {
  try {
    return JSON.parse(result.stdout) as HeddleVerbOutputs[V];
  } catch {
    throw new HeddleError({
      verb,
      exitCode: result.exitCode,
      stdout: result.stdout,
      stderr: result.stderr,
    });
  }
}

/**
 * The CLI emits its JSON error envelope on STDERR in `--output json` mode
 * (success payloads go to stdout). Best-effort parse; undefined if the
 * stderr wasn't a JSON envelope (e.g. a panic or clap usage error).
 */
function parseErrorEnvelope(result: ExecResult): ErrorEnvelopeSchema | undefined {
  const text = result.stderr.trim();
  if (!text) return undefined;
  try {
    const parsed = JSON.parse(text) as ErrorEnvelopeSchema;
    return typeof parsed === "object" && parsed !== null && "exit_code" in parsed
      ? parsed
      : undefined;
  } catch {
    return undefined;
  }
}
