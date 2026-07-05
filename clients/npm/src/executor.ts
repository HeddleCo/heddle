import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import { HeddleError, HeddleExitCode } from "./errors.js";

const DEFAULT_MAX_OUTPUT_BYTES = 64 * 1024 * 1024;

/**
 * One CLI invocation, transport-neutral. The `verb` is the canonical
 * space-joined verb (e.g. `"export git"`); the executor is
 * responsible for splitting it into argv tokens for a subprocess, or
 * routing it however a daemon/napi backend prefers.
 */
export interface ExecRequest {
  /** Canonical verb, space-joined (e.g. "export git"). */
  verb: string;
  /** Verb-specific arguments appended after the global flags. */
  args: readonly string[];
  /** Threaded as `--op-id` for idempotent retries on mutating verbs. */
  opId?: string | undefined;
  /** Repo path, threaded as `-C <path>` (default: process cwd). */
  repoPath?: string | undefined;
  /** Abort signal to cancel an in-flight invocation. */
  signal?: AbortSignal | undefined;
}

/** Raw, unparsed result of an invocation. Parsing lives in `Heddle`. */
export interface ExecResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

/**
 * Thrown by {@link Executor.execStream} when a streaming invocation exits
 * non-zero. Carries the same {@link ExecResult} shape (the accumulated
 * stdout lines + stderr + code) so {@link Heddle.stream} can rebuild the
 * usual {@link HeddleError}.
 */
export class ExecStreamError extends Error {
  readonly result: ExecResult;
  constructor(result: ExecResult) {
    super(`heddle stream exited with code ${result.exitCode}`);
    this.name = "ExecStreamError";
    this.result = result;
  }
}

/**
 * The transport seam. The default {@link SpawnExecutor} shells out to the
 * `heddle` binary, but a future napi or daemon backend (#586) can implement
 * this same interface and swap in with no change to `Heddle` call sites —
 * it just has to return the JSON contract on stdout and a sysexits code.
 */
export interface Executor {
  exec(req: ExecRequest): Promise<ExecResult>;
  /**
   * Optional streaming transport for JSONL verbs. Yields each stdout line
   * (newline stripped) as it arrives — without waiting for the process to
   * exit — so {@link Heddle.stream} can drive indefinite verbs like
   * `heddle watch` / `status --watch`. The iterator completes when the
   * child's stdout closes; on a non-zero exit it throws
   * {@link ExecStreamError}. Executors that can't stream (e.g. a buffered
   * daemon backend) omit this; {@link Heddle.stream} falls back to
   * {@link Executor.exec} and splits the buffered output.
   */
  execStream?(req: ExecRequest): AsyncIterable<string>;
}

export interface SpawnExecutorOptions {
  /** Path to the heddle binary. Defaults to "heddle" (resolved on PATH). */
  binaryPath?: string | undefined;
  /** Default repo path applied when a request omits `repoPath`. */
  repoPath?: string | undefined;
  /** Working directory for the spawned process. */
  cwd?: string | undefined;
  /** Extra environment overlaid on `process.env`. */
  env?: Record<string, string> | undefined;
  /**
   * Maximum buffered stdout + stderr per invocation. Defaults to 64 MiB:
   * high enough for large JSON envelopes/diffs, but finite so a bad verb or
   * unexpected stream cannot grow the Node process without bound.
   */
  maxOutputBytes?: number | undefined;
}

/**
 * Default executor: spawns `heddle <verb> --output json [...]` as a
 * subprocess and captures stdout/stderr. Binary-bundling/resolution is
 * out of scope (#584) — the caller supplies `binaryPath` or relies on PATH.
 */
export class SpawnExecutor implements Executor {
  private readonly binaryPath: string;
  private readonly repoPath: string | undefined;
  private readonly cwd: string | undefined;
  private readonly env: Record<string, string> | undefined;
  private readonly maxOutputBytes: number;

  constructor(options: SpawnExecutorOptions = {}) {
    this.binaryPath = options.binaryPath ?? "heddle";
    this.repoPath = options.repoPath;
    this.cwd = options.cwd;
    this.env = options.env;
    this.maxOutputBytes = options.maxOutputBytes ?? DEFAULT_MAX_OUTPUT_BYTES;
  }

  exec(req: ExecRequest): Promise<ExecResult> {
    const argv = this.buildArgv(req);
    const spawnOptions: Parameters<typeof spawn>[2] = {
      env: this.env ? { ...process.env, ...this.env } : process.env,
    };
    if (this.cwd !== undefined) spawnOptions.cwd = this.cwd;
    if (req.signal !== undefined) spawnOptions.signal = req.signal;

    return new Promise<ExecResult>((resolve, reject) => {
      const child = spawn(this.binaryPath, argv, spawnOptions);
      const stdout: Buffer[] = [];
      const stderr: Buffer[] = [];
      let totalOutputBytes = 0;
      let settled = false;
      const fail = (err: Error) => {
        if (settled) return;
        settled = true;
        reject(err);
        if (child.exitCode === null && child.signalCode === null) {
          child.kill("SIGTERM");
        }
      };
      const collect = (stream: "stdout" | "stderr", chunk: Buffer) => {
        totalOutputBytes += chunk.byteLength;
        if (totalOutputBytes > this.maxOutputBytes) {
          fail(this.outputTooLargeError(req.verb, totalOutputBytes));
          return;
        }
        if (stream === "stdout") stdout.push(chunk);
        else stderr.push(chunk);
      };
      child.stdout?.on("data", (chunk: Buffer) => collect("stdout", chunk));
      child.stderr?.on("data", (chunk: Buffer) => collect("stderr", chunk));
      child.on("error", (err: Error) => {
        fail(this.spawnError(req.verb, err));
      });
      child.on("close", (code, signal) => {
        if (settled) return;
        settled = true;
        resolve({
          // A signal kill reports `code === null`; surface 128+signal-ish
          // as a generic IO failure so callers still get a non-zero code.
          exitCode: code ?? (signal ? 128 : 1),
          stdout: Buffer.concat(stdout).toString("utf8"),
          stderr: Buffer.concat(stderr).toString("utf8"),
        });
      });
    });
  }

  async *execStream(req: ExecRequest): AsyncGenerator<string, void, unknown> {
    const argv = this.buildArgv(req);
    const spawnOptions: Parameters<typeof spawn>[2] = {
      env: this.env ? { ...process.env, ...this.env } : process.env,
    };
    if (this.cwd !== undefined) spawnOptions.cwd = this.cwd;
    if (req.signal !== undefined) spawnOptions.signal = req.signal;

    const child = spawn(this.binaryPath, argv, spawnOptions);
    const stderr: Buffer[] = [];
    let totalOutputBytes = 0;
    let streamFailure: HeddleError | undefined;
    const trackOutput = (bytes: number) => {
      totalOutputBytes += bytes;
      if (totalOutputBytes > this.maxOutputBytes) {
        streamFailure = this.outputTooLargeError(req.verb, totalOutputBytes);
        child.kill("SIGTERM");
      }
    };
    child.stderr?.on("data", (chunk: Buffer) => {
      trackOutput(chunk.byteLength);
      if (!streamFailure) stderr.push(chunk);
    });
    const childExit = this.waitForExit(child);
    let spawnError: HeddleError | undefined;
    child.on("error", (err: Error) => {
      spawnError = this.spawnError(req.verb, err);
    });

    const stdoutLines: string[] = [];
    let stdoutDone = false;
    try {
      if (child.stdout) {
        const rl = createInterface({ input: child.stdout, crlfDelay: Infinity });
        // readline yields each line as it arrives on stdout — the loop
        // produces values during the child's lifetime, not after exit.
        for await (const line of rl) {
          trackOutput(Buffer.byteLength(line, "utf8") + 1);
          if (streamFailure) throw streamFailure;
          stdoutLines.push(line);
          yield line;
        }
      }
      stdoutDone = true;
    } finally {
      if (!stdoutDone) {
        await this.terminateChild(child, childExit);
      }
    }

    const exitCode = await childExit;

    if (streamFailure) throw streamFailure;
    if (spawnError) throw spawnError;
    if (exitCode !== 0) {
      throw new ExecStreamError({
        exitCode,
        stdout: stdoutLines.join("\n"),
        stderr: Buffer.concat(stderr).toString("utf8"),
      });
    }
  }

  /** `[--output json, -C <repo>?, --op-id <id>?, ...verb tokens, ...args]`. */
  private buildArgv(req: ExecRequest): string[] {
    const argv: string[] = ["--output", "json"];
    const repoPath = req.repoPath ?? this.repoPath;
    if (repoPath !== undefined) argv.push("-C", repoPath);
    if (req.opId !== undefined) argv.push("--op-id", req.opId);
    argv.push(...req.verb.split(" "));
    argv.push(...req.args);
    return argv;
  }

  private waitForExit(child: ReturnType<typeof spawn>): Promise<number> {
    return new Promise<number>((resolve) => {
      const settle = (code: number | null, signal: NodeJS.Signals | null) =>
        resolve(code ?? (signal ? 128 : 1));
      if (child.exitCode !== null || child.signalCode !== null) {
        settle(child.exitCode, child.signalCode);
      } else {
        child.once("close", settle);
      }
    });
  }

  private async terminateChild(
    child: ReturnType<typeof spawn>,
    childExit: Promise<number>,
  ): Promise<void> {
    if (child.exitCode !== null || child.signalCode !== null) {
      await childExit;
      return;
    }

    child.kill("SIGTERM");
    const killTimer = setTimeout(() => {
      if (child.exitCode === null && child.signalCode === null) {
        child.kill("SIGKILL");
      }
    }, 250);
    try {
      await childExit;
    } finally {
      clearTimeout(killTimer);
    }
  }

  private spawnError(verb: string, err: Error): HeddleError {
    const nodeCode = typeof (err as NodeJS.ErrnoException).code === "string"
      ? (err as NodeJS.ErrnoException).code
      : undefined;
    const kind = nodeCode === "ENOENT"
      ? "binary_not_found"
      : nodeCode === "EACCES"
        ? "binary_not_executable"
        : "spawn_failed";
    return new HeddleError({
      verb,
      exitCode: HeddleExitCode.IoErr,
      stdout: "",
      stderr: err.message,
      envelope: {
        error: err.message,
        exit_code: HeddleExitCode.IoErr,
        hint: nodeCode === "ENOENT"
          ? "Install heddle or pass binaryPath to the Heddle client."
          : "Check that the heddle binary can be executed.",
        kind,
        preserved: "no repository objects, refs, metadata, or worktree files were changed",
        primary_command: "heddle --version",
        primary_command_template: null,
        recovery_action_templates: [],
        recovery_commands: ["heddle --version"],
        unsafe_condition: `the client could not start the heddle binary`,
        would_change: "the command did not start",
      },
    });
  }

  private outputTooLargeError(verb: string, bytes: number): HeddleError {
    return new HeddleError({
      verb,
      exitCode: HeddleExitCode.IoErr,
      stdout: "",
      stderr: `heddle ${verb} exceeded buffered output limit of ${this.maxOutputBytes} bytes`,
      envelope: {
        error: `heddle ${verb} exceeded buffered output limit of ${this.maxOutputBytes} bytes`,
        exit_code: HeddleExitCode.IoErr,
        hint: "Use a narrower query or a streaming API for unbounded output.",
        kind: "output_too_large",
        preserved: "no buffered output beyond the configured client limit was retained",
        primary_command: `heddle ${verb}`,
        primary_command_template: null,
        recovery_action_templates: [],
        recovery_commands: [],
        unsafe_condition: `the invocation emitted ${bytes} bytes, above the client buffer limit`,
        would_change: "continuing to buffer output could exhaust the Node process memory",
      },
    });
  }
}
