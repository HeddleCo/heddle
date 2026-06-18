export {
  Heddle,
  HEDDLE_STREAMING_VERBS,
  HEDDLE_WATCH_MODE_VERBS,
  HEDDLE_WATCH_FLAGS,
} from "./heddle.js";
export type {
  HeddleOptions,
  RunOptions,
  HeddleStreamingVerb,
  HeddleWatchModeVerb,
  HeddleJsonlVerb,
  HeddleRunVerb,
  TimelineBranchReason,
  TimelineForkOptions,
  TimelineLogOptions,
  TimelineMaterializeMode,
  TimelineRecoverOptions,
  TimelineResetOptions,
  TimelineTarget,
} from "./heddle.js";
export {
  SpawnExecutor,
  ExecStreamError,
  type Executor,
  type ExecRequest,
  type ExecResult,
  type SpawnExecutorOptions,
} from "./executor.js";
export {
  DEFAULT_NATIVE_TOOL_CALL_HARNESS,
  DEFAULT_TIMELINE_BRANCH_REASON,
  DEFAULT_TIMELINE_THREAD,
  HeddleTimelineDaemon,
  TimelineMaterializeModeCode,
  forkTimelineFromSelectorRequest,
  getTimelineNavigationRequest,
  recoverTimelineMaterializationRequest,
  resetTimelineCursorRequest,
  timelineMaterializeModeToCode,
  timelineTargetToSeekSelector,
} from "./timeline-daemon.js";
export type {
  ForkTimelineFromSelectorRequest,
  ForkTimelineFromSelectorResponse,
  GetTimelineNavigationRequest,
  HeddleTimelineDaemonOptions,
  RecoverTimelineMaterializationRequest,
  RecoverTimelineMaterializationResponse,
  ResetTimelineCursorRequest,
  ResetTimelineCursorResponse,
  TimelineCursorSelector,
  TimelineDaemonCallOptions,
  TimelineDaemonRecord,
  TimelineDaemonTransport,
  TimelineNavigationSnapshot,
  TimelineSeekNativeToolCallSelector,
  TimelineSeekSelector,
  TimelineSeekStepSelector,
  TimelineTargetSelectorOptions,
} from "./timeline-daemon.js";
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
