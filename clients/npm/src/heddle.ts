import type {
  HeddleSchemaVerb,
  HeddleVerbOutputs,
  ErrorEnvelopeSchema,
} from "../generated/heddle-schemas.js";
import {
  SpawnExecutor,
  type Executor,
  type ExecResult,
} from "./executor.js";
import { HeddleError } from "./errors.js";

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
   * Run any schema-backed verb and return its typed payload. Throws
   * {@link HeddleError} on a non-zero exit, {@link HeddleError} on
   * unparseable stdout.
   */
  async run<V extends HeddleSchemaVerb>(
    verb: V,
    args: readonly string[] = [],
    options: RunOptions = {},
  ): Promise<HeddleVerbOutputs[V]> {
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
