import { test } from "node:test";
import assert from "node:assert/strict";
import {
  HeddleTimelineDaemon,
  TimelineMaterializeModeCode,
  forkTimelineFromSelectorRequest,
  getTimelineNavigationRequest,
  recoverTimelineMaterializationRequest,
  resetTimelineCursorRequest,
  timelineTargetToSeekSelector,
  type ForkTimelineFromSelectorRequest,
  type ForkTimelineFromSelectorResponse,
  type GetTimelineNavigationRequest,
  type RecoverTimelineMaterializationRequest,
  type RecoverTimelineMaterializationResponse,
  type ResetTimelineCursorRequest,
  type ResetTimelineCursorResponse,
  type TimelineDaemonTransport,
  type TimelineNavigationSnapshot,
} from "../src/timeline-daemon.js";

class FakeTimelineTransport implements TimelineDaemonTransport {
  calls: Array<{ method: string; request: unknown }> = [];

  async GetTimelineNavigation(
    request: GetTimelineNavigationRequest,
  ): Promise<TimelineNavigationSnapshot> {
    this.calls.push({ method: "GetTimelineNavigation", request });
    return { thread: request.thread };
  }

  async ForkTimelineFromSelector(
    request: ForkTimelineFromSelectorRequest,
  ): Promise<ForkTimelineFromSelectorResponse> {
    this.calls.push({ method: "ForkTimelineFromSelector", request });
    return {
      branch_id: request.branch_id,
      parent_branch_id: "tlb-main",
      from_step_id: "tls-one",
    };
  }

  async ResetTimelineCursor(
    request: ResetTimelineCursorRequest,
  ): Promise<ResetTimelineCursorResponse> {
    this.calls.push({ method: "ResetTimelineCursor", request });
    return { navigation: { materialized: request.materialize_checkout } };
  }

  async RecoverTimelineMaterialization(
    request: RecoverTimelineMaterializationRequest,
  ): Promise<RecoverTimelineMaterializationResponse> {
    this.calls.push({ method: "RecoverTimelineMaterialization", request });
    return {
      recovery_status: 1,
      recovery_blockers: [],
    };
  }
}

test("timelineTargetToSeekSelector maps step targets into proto oneof shape", () => {
  assert.deepEqual(
    timelineTargetToSeekSelector({
      kind: "step",
      thread: "feature",
      fromBranch: "tlb-parent",
      stepId: "tls-one",
    }),
    {
      target: {
        step: {
          thread: "feature",
          branch_id: "tlb-parent",
          step_id: "tls-one",
        },
      },
    },
  );
});

test("timelineTargetToSeekSelector defaults native tool calls to OpenCode", () => {
  assert.deepEqual(
    timelineTargetToSeekSelector({
      kind: "tool-call",
      toolCallId: "call-123",
      sessionId: "session-456",
      messageId: "message-789",
    }),
    {
      target: {
        native_tool_call: {
          thread: "main",
          harness: "opencode",
          session_id: "session-456",
          message_id: "message-789",
          tool_call_id: "call-123",
        },
      },
    },
  );
});

test("timelineTargetToSeekSelector maps cursor selectors with branch constraints", () => {
  assert.deepEqual(
    timelineTargetToSeekSelector({
      kind: "undo",
      thread: "main",
      fromBranch: "tlb-main",
    }),
    {
      target: {
        undo: {
          thread: "main",
          branch_id: "tlb-main",
        },
      },
    },
  );

  assert.deepEqual(
    timelineTargetToSeekSelector({ kind: "current" }, { thread: "review" }),
    {
      target: {
        current_cursor: {
          thread: "review",
          branch_id: "",
        },
      },
    },
  );
});

test("request builders model TimelineService request payloads", () => {
  assert.deepEqual(
    getTimelineNavigationRequest({ thread: "main" }, { repoPath: "/repo" }),
    {
      repo_path: "/repo",
      thread: "main",
    },
  );

  assert.deepEqual(
    forkTimelineFromSelectorRequest(
      { kind: "tool-call", thread: "main", toolCallId: "call-1" },
      { branch: "tlb-child", reason: "fan-out" },
      { repoPath: "/repo", opId: "op-fork" },
    ),
    {
      repo_path: "/repo",
      selector: {
        target: {
          native_tool_call: {
            thread: "main",
            harness: "opencode",
            session_id: "",
            message_id: "",
            tool_call_id: "call-1",
          },
        },
      },
      branch_id: "tlb-child",
      reason: "fan-out",
      client_operation_id: "op-fork",
    },
  );

  assert.deepEqual(
    resetTimelineCursorRequest(
      { kind: "step", thread: "main", stepId: "tls-one" },
      { materialize: true, mode: "capture-current-then-seek" },
      { repoPath: "/repo", opId: "op-reset" },
    ),
    {
      repo_path: "/repo",
      selector: {
        target: {
          step: {
            thread: "main",
            branch_id: "",
            step_id: "tls-one",
          },
        },
      },
      mode: TimelineMaterializeModeCode.CaptureCurrentThenSeek,
      materialize_checkout: true,
      client_operation_id: "op-reset",
    },
  );

  assert.deepEqual(
    recoverTimelineMaterializationRequest(
      { thread: "main" },
      { repoPath: "/repo", opId: "op-recover" },
    ),
    {
      repo_path: "/repo",
      thread: "main",
      client_operation_id: "op-recover",
    },
  );
});

test("HeddleTimelineDaemon dispatches to TimelineService transport methods", async () => {
  const fake = new FakeTimelineTransport();
  const client = new HeddleTimelineDaemon(fake, {
    repoPath: "/repo",
    thread: "main",
  });

  await client.getTimelineNavigation();
  await client.forkTimelineFromSelector(
    { kind: "tool-call", toolCallId: "call-1" },
    { branch: "tlb-child" },
    { opId: "op-fork" },
  );
  await client.resetTimelineCursor(
    { kind: "redo" },
    { materialize: true },
    { opId: "op-reset" },
  );
  await client.recoverTimelineMaterialization({}, { opId: "op-recover" });

  assert.deepEqual(
    fake.calls.map((call) => call.method),
    [
      "GetTimelineNavigation",
      "ForkTimelineFromSelector",
      "ResetTimelineCursor",
      "RecoverTimelineMaterialization",
    ],
  );
  assert.deepEqual(fake.calls[0]?.request, {
    repo_path: "/repo",
    thread: "main",
  });
  assert.deepEqual(fake.calls[1]?.request, {
    repo_path: "/repo",
    selector: {
      target: {
        native_tool_call: {
          thread: "main",
          harness: "opencode",
          session_id: "",
          message_id: "",
          tool_call_id: "call-1",
        },
      },
    },
    branch_id: "tlb-child",
    reason: "explicit-fork",
    client_operation_id: "op-fork",
  });
  assert.deepEqual(fake.calls[2]?.request, {
    repo_path: "/repo",
    selector: {
      target: {
        redo: {
          thread: "main",
          branch_id: "",
        },
      },
    },
    mode: TimelineMaterializeModeCode.FailIfDirty,
    materialize_checkout: true,
    client_operation_id: "op-reset",
  });
  assert.deepEqual(fake.calls[3]?.request, {
    repo_path: "/repo",
    thread: "main",
    client_operation_id: "op-recover",
  });
});
