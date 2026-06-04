# Collaboration sync lane

Collaboration operations sync through the same repository remote UX as source history, but over a distinct protocol lane from immutable source objects. Normal `push`, `pull`, and `fetch` should include collaboration operations by default, while advanced collaboration-only sync can exist for repair or debugging.

A source `push` that succeeds for source objects but fails to sync linked collaboration records should fail by default with clear partial-lane reporting. Linked discussions, context, and attention may be needed to understand the pushed content, so silent collaboration loss is not acceptable. Operators can use an explicit `--allow-partial` override when they intentionally want the source lane to proceed despite collaboration sync failure.

If a later retry finds the source lane already up to date, it should focus on the failed collaboration lane while still verifying the source target has not moved incompatibly. The user-facing command can remain `heddle push`; internally the retry is lane-aware and makes collaboration recovery explicit.

Retrying failed linked collaboration sync does not require the exact capability context used for the original source push. It requires a current capability context that is valid for the collaboration records being synced. A stronger, renewed, or otherwise corrected capability can complete the collaboration lane, and sync metadata records the capability context Weft accepted.

The collaboration operation envelope records the operation capability context from creation time. The collaboration sync lane records the hosted acceptance context separately in sync metadata for the specific remote. These may differ when sync is retried after grants or capabilities change.

When Weft reports that hosted policy or grants changed, the sync result should perform automatic capability refresh and tell the user their permission scope changed. If the refreshed root capability narrows, derived local capability context is automatically capped by the refreshed root so future collaboration operations and sync attempts use policy context that matches current hosted policy.

**Status:** accepted

**Considered Options:** A separate Weft-only collaboration channel would simplify live hosted features, but it would make durable discussions feel like a separate product from repository synchronization. Folding collaboration operations into the exact same object lane as source history would blur very different consistency and cursor requirements.
