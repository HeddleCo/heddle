export { Heddle, HEDDLE_STREAMING_VERBS } from "./heddle.js";
export type {
  HeddleOptions,
  RunOptions,
  HeddleStreamingVerb,
  HeddleRunVerb,
} from "./heddle.js";
export {
  SpawnExecutor,
  type Executor,
  type ExecRequest,
  type ExecResult,
  type SpawnExecutorOptions,
} from "./executor.js";
export {
  HeddleError,
  HeddleStreamingVerbError,
  HeddleExitCode,
  RETRYABLE_EXIT_CODE,
  type HeddleExitCodeName,
} from "./errors.js";

// Re-export the generated contract so callers can `import { Heddle, type
// StatusSchema } from "@heddle/cli"` without reaching into generated/.
export * from "../generated/heddle-schemas.js";
