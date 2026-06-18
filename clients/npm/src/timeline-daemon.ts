import type {
  RunOptions,
  TimelineForkOptions,
  TimelineLogOptions,
  TimelineMaterializeMode,
  TimelineRecoverOptions,
  TimelineResetOptions,
  TimelineTarget,
} from "./heddle.js";

export const DEFAULT_TIMELINE_THREAD = "main";
export const DEFAULT_NATIVE_TOOL_CALL_HARNESS = "opencode";
export const DEFAULT_TIMELINE_BRANCH_REASON = "explicit-fork";

export const TimelineMaterializeModeCode = {
  Unspecified: 0,
  FailIfDirty: 1,
  CaptureCurrentThenSeek: 2,
} as const;

export type TimelineMaterializeModeCode =
  (typeof TimelineMaterializeModeCode)[keyof typeof TimelineMaterializeModeCode];

export interface TimelineDaemonCallOptions extends Pick<RunOptions, "opId" | "repoPath"> {}

export interface TimelineSeekStepSelector {
  thread: string;
  branch_id: string;
  step_id: string;
}

export interface TimelineSeekNativeToolCallSelector {
  thread: string;
  harness: string;
  session_id: string;
  message_id: string;
  tool_call_id: string;
}

export interface TimelineCursorSelector {
  thread: string;
  branch_id: string;
}

export type TimelineSeekSelector =
  | { target: { step: TimelineSeekStepSelector } }
  | { target: { native_tool_call: TimelineSeekNativeToolCallSelector } }
  | { target: { undo: TimelineCursorSelector } }
  | { target: { redo: TimelineCursorSelector } }
  | { target: { current_cursor: TimelineCursorSelector } };

export interface TimelineTargetSelectorOptions {
  /** Default timeline thread when the target omits one. Defaults to "main". */
  thread?: string | undefined;
  /** Default native harness for tool-call selectors. Defaults to "opencode". */
  harness?: string | undefined;
}

export interface GetTimelineNavigationRequest {
  repo_path: string;
  thread: string;
}

export interface ForkTimelineFromSelectorRequest {
  repo_path: string;
  selector: TimelineSeekSelector;
  branch_id: string;
  reason: string;
  client_operation_id: string;
}

export interface ResetTimelineCursorRequest {
  repo_path: string;
  selector: TimelineSeekSelector;
  mode: TimelineMaterializeModeCode;
  materialize_checkout: boolean;
  client_operation_id: string;
}

export interface RecoverTimelineMaterializationRequest {
  repo_path: string;
  thread: string;
  client_operation_id: string;
}

export type TimelineDaemonRecord = Record<string, unknown>;

export interface ForkTimelineFromSelectorResponse {
  navigation?: TimelineDaemonRecord | undefined;
  operation?: TimelineDaemonRecord | undefined;
  branch_id: string;
  parent_branch_id: string;
  from_step_id: string;
}

export interface ResetTimelineCursorResponse {
  navigation?: TimelineDaemonRecord | undefined;
  cursor_operation?: TimelineDaemonRecord | undefined;
  materialization?: TimelineDaemonRecord | undefined;
}

export interface RecoverTimelineMaterializationResponse {
  navigation?: TimelineDaemonRecord | undefined;
  recovered_cursor_operation?: TimelineDaemonRecord | undefined;
  recovery_status: number;
  recovery_blockers: TimelineDaemonRecord[];
}

export type TimelineNavigationSnapshot = TimelineDaemonRecord;

export interface TimelineDaemonTransport {
  GetTimelineNavigation(request: GetTimelineNavigationRequest): Promise<TimelineNavigationSnapshot>;
  ForkTimelineFromSelector(
    request: ForkTimelineFromSelectorRequest,
  ): Promise<ForkTimelineFromSelectorResponse>;
  ResetTimelineCursor(request: ResetTimelineCursorRequest): Promise<ResetTimelineCursorResponse>;
  RecoverTimelineMaterialization(
    request: RecoverTimelineMaterializationRequest,
  ): Promise<RecoverTimelineMaterializationResponse>;
}

export interface HeddleTimelineDaemonOptions {
  /** Instance default repo path, passed through as proto `repo_path`. */
  repoPath?: string | undefined;
  /** Instance default thread when call args or targets omit one. Defaults to "main". */
  thread?: string | undefined;
  /** Instance default native tool-call harness. Defaults to "opencode". */
  harness?: string | undefined;
}

export class HeddleTimelineDaemon {
  private readonly repoPath: string;
  private readonly thread: string;
  private readonly harness: string;

  constructor(
    private readonly transport: TimelineDaemonTransport,
    options: HeddleTimelineDaemonOptions = {},
  ) {
    this.repoPath = options.repoPath ?? "";
    this.thread = options.thread ?? DEFAULT_TIMELINE_THREAD;
    this.harness = options.harness ?? DEFAULT_NATIVE_TOOL_CALL_HARNESS;
  }

  getTimelineNavigation(
    args: TimelineLogOptions = {},
    options: TimelineDaemonCallOptions = {},
  ): Promise<TimelineNavigationSnapshot> {
    return this.transport.GetTimelineNavigation(
      getTimelineNavigationRequest(args, this.requestOptions(options)),
    );
  }

  forkTimelineFromSelector(
    target: TimelineTarget,
    args: TimelineForkOptions = {},
    options: TimelineDaemonCallOptions = {},
  ): Promise<ForkTimelineFromSelectorResponse> {
    return this.transport.ForkTimelineFromSelector(
      forkTimelineFromSelectorRequest(target, args, this.requestOptions(options)),
    );
  }

  resetTimelineCursor(
    target: TimelineTarget,
    args: TimelineResetOptions = {},
    options: TimelineDaemonCallOptions = {},
  ): Promise<ResetTimelineCursorResponse> {
    return this.transport.ResetTimelineCursor(
      resetTimelineCursorRequest(target, args, this.requestOptions(options)),
    );
  }

  recoverTimelineMaterialization(
    args: TimelineRecoverOptions = {},
    options: TimelineDaemonCallOptions = {},
  ): Promise<RecoverTimelineMaterializationResponse> {
    return this.transport.RecoverTimelineMaterialization(
      recoverTimelineMaterializationRequest(args, this.requestOptions(options)),
    );
  }

  private requestOptions(options: TimelineDaemonCallOptions): Required<TimelineRequestOptions> {
    return {
      repoPath: options.repoPath ?? this.repoPath,
      opId: options.opId ?? "",
      thread: this.thread,
      harness: this.harness,
    };
  }
}

interface TimelineRequestOptions extends TimelineTargetSelectorOptions {
  repoPath?: string | undefined;
  opId?: string | undefined;
}

export function timelineTargetToSeekSelector(
  target: TimelineTarget,
  options: TimelineTargetSelectorOptions = {},
): TimelineSeekSelector {
  const thread = target.thread ?? options.thread ?? DEFAULT_TIMELINE_THREAD;

  switch (target.kind) {
    case "step":
      return {
        target: {
          step: {
            thread,
            branch_id: target.fromBranch ?? "",
            step_id: target.stepId,
          },
        },
      };
    case "tool-call":
      return {
        target: {
          native_tool_call: {
            thread,
            harness: target.harness ?? options.harness ?? DEFAULT_NATIVE_TOOL_CALL_HARNESS,
            session_id: target.sessionId ?? "",
            message_id: target.messageId ?? "",
            tool_call_id: target.toolCallId,
          },
        },
      };
    case "undo":
      return cursorSelector("undo", thread, target.fromBranch);
    case "redo":
      return cursorSelector("redo", thread, target.fromBranch);
    case "current":
      return cursorSelector("current_cursor", thread, target.fromBranch);
  }
}

export function getTimelineNavigationRequest(
  args: TimelineLogOptions = {},
  options: TimelineRequestOptions = {},
): GetTimelineNavigationRequest {
  return {
    repo_path: options.repoPath ?? "",
    thread: args.thread ?? options.thread ?? DEFAULT_TIMELINE_THREAD,
  };
}

export function forkTimelineFromSelectorRequest(
  target: TimelineTarget,
  args: TimelineForkOptions = {},
  options: TimelineRequestOptions = {},
): ForkTimelineFromSelectorRequest {
  return {
    repo_path: options.repoPath ?? "",
    selector: timelineTargetToSeekSelector(target, options),
    branch_id: args.branch ?? "",
    reason: args.reason ?? DEFAULT_TIMELINE_BRANCH_REASON,
    client_operation_id: options.opId ?? "",
  };
}

export function resetTimelineCursorRequest(
  target: TimelineTarget,
  args: TimelineResetOptions = {},
  options: TimelineRequestOptions = {},
): ResetTimelineCursorRequest {
  return {
    repo_path: options.repoPath ?? "",
    selector: timelineTargetToSeekSelector(target, options),
    mode: timelineMaterializeModeToCode(args.mode),
    materialize_checkout: args.materialize === true,
    client_operation_id: options.opId ?? "",
  };
}

export function recoverTimelineMaterializationRequest(
  args: TimelineRecoverOptions = {},
  options: TimelineRequestOptions = {},
): RecoverTimelineMaterializationRequest {
  return {
    repo_path: options.repoPath ?? "",
    thread: args.thread ?? options.thread ?? DEFAULT_TIMELINE_THREAD,
    client_operation_id: options.opId ?? "",
  };
}

export function timelineMaterializeModeToCode(
  mode: TimelineMaterializeMode | undefined,
): TimelineMaterializeModeCode {
  switch (mode) {
    case undefined:
    case "fail-if-dirty":
      return TimelineMaterializeModeCode.FailIfDirty;
    case "capture-current-then-seek":
      return TimelineMaterializeModeCode.CaptureCurrentThenSeek;
  }
}

function cursorSelector(
  kind: "undo" | "redo" | "current_cursor",
  thread: string,
  branchId: string | undefined,
): TimelineSeekSelector {
  return {
    target: {
      [kind]: {
        thread,
        branch_id: branchId ?? "",
      },
    },
  } as TimelineSeekSelector;
}
