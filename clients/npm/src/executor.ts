import { spawn } from "node:child_process";
import { createInterface } from "node:readline";

/**
 * One CLI invocation, transport-neutral. The `verb` is the canonical
 * space-joined verb (e.g. `"bridge git export"`); the executor is
 * responsible for splitting it into argv tokens for a subprocess, or
 * routing it however a daemon/napi backend prefers.
 */
export interface ExecRequest {
  /** Canonical verb, space-joined (e.g. "bridge git export"). */
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

  constructor(options: SpawnExecutorOptions = {}) {
    this.binaryPath = options.binaryPath ?? "heddle";
    this.repoPath = options.repoPath;
    this.cwd = options.cwd;
    this.env = options.env;
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
      child.stdout?.on("data", (chunk: Buffer) => stdout.push(chunk));
      child.stderr?.on("data", (chunk: Buffer) => stderr.push(chunk));
      child.on("error", reject);
      child.on("close", (code, signal) => {
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
    child.stderr?.on("data", (chunk: Buffer) => stderr.push(chunk));
    let spawnError: Error | undefined;
    child.on("error", (err: Error) => {
      spawnError = err;
    });

    const stdoutLines: string[] = [];
    if (child.stdout) {
      const rl = createInterface({ input: child.stdout, crlfDelay: Infinity });
      // readline yields each line as it arrives on stdout — the loop
      // produces values during the child's lifetime, not after exit.
      for await (const line of rl) {
        stdoutLines.push(line);
        yield line;
      }
    }

    const exitCode = await new Promise<number>((resolve) => {
      const settle = (code: number | null, signal: NodeJS.Signals | null) =>
        resolve(code ?? (signal ? 128 : 1));
      if (child.exitCode !== null || child.signalCode !== null) {
        settle(child.exitCode, child.signalCode);
      } else {
        child.once("close", settle);
      }
    });

    if (spawnError) throw spawnError;
    if (exitCode !== 0) {
      throw new ExecStreamError({
        exitCode,
        stdout: stdoutLines.join("\n"),
        stderr: Buffer.concat(stderr).toString("utf8"),
      });
    }
  }

  /** `[...verb tokens, --output json, -C <repo>?, --op-id <id>?, ...args]`. */
  private buildArgv(req: ExecRequest): string[] {
    const argv: string[] = [...req.verb.split(" "), "--output", "json"];
    const repoPath = req.repoPath ?? this.repoPath;
    if (repoPath !== undefined) argv.push("-C", repoPath);
    if (req.opId !== undefined) argv.push("--op-id", req.opId);
    argv.push(...req.args);
    return argv;
  }
}
