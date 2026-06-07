import type { ErrorEnvelopeSchema } from "../generated/heddle-schemas.js";

/**
 * BSD sysexits codes the heddle CLI emits. The canonical table lives in
 * `docs/exit-codes.md`; mirrored here so callers can branch without magic
 * numbers. `TempFail` (75) is the ONLY safe-to-retry code.
 */
export const HeddleExitCode = {
  Ok: 0,
  Usage: 64,
  DataErr: 65,
  CantCreat: 73,
  IoErr: 74,
  TempFail: 75,
  Protocol: 76,
  NoPerm: 77,
  Config: 78,
} as const;

export type HeddleExitCodeName = keyof typeof HeddleExitCode;

/** The one exit code for which retrying with identical args is safe. */
export const RETRYABLE_EXIT_CODE = HeddleExitCode.TempFail;

/**
 * Thrown when a heddle invocation exits non-zero. Carries the parsed JSON
 * error envelope (when the CLI emitted one), the raw streams, and a
 * `retryable` flag derived from the sysexits contract (only exit 75).
 */
export class HeddleError extends Error {
  /** Process exit code (sysexits). */
  readonly exitCode: number;
  /** Stable error `kind`/`code` from the envelope, if present. */
  readonly code: string | undefined;
  /** Safe to retry with the same args? True only for exit 75 (TempFail). */
  readonly retryable: boolean;
  /** Parsed `--output json` error envelope, if the CLI emitted one. */
  readonly envelope: ErrorEnvelopeSchema | undefined;
  /** Raw stderr (the envelope is emitted here in JSON mode). */
  readonly stderr: string;
  /** Raw stdout (usually empty on failure). */
  readonly stdout: string;
  /** The verb that was invoked, e.g. "bridge git export". */
  readonly verb: string;

  constructor(args: {
    verb: string;
    exitCode: number;
    stdout: string;
    stderr: string;
    envelope?: ErrorEnvelopeSchema | undefined;
  }) {
    const { verb, exitCode, stdout, stderr, envelope } = args;
    const message =
      envelope?.error ??
      stderr.trim() ??
      `heddle ${verb} exited with code ${exitCode}`;
    super(message || `heddle ${verb} exited with code ${exitCode}`);
    this.name = "HeddleError";
    this.verb = verb;
    this.exitCode = exitCode;
    this.stdout = stdout;
    this.stderr = stderr;
    this.envelope = envelope;
    this.code = envelope?.code ?? envelope?.kind;
    this.retryable = exitCode === RETRYABLE_EXIT_CODE;
  }
}
