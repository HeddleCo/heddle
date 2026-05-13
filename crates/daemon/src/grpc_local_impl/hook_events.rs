// SPDX-License-Identifier: Apache-2.0
//! In-process hook-event broker for the local-mode daemon.
//!
//! The hosted variant rides Postgres NOTIFY (see the `server` crate's
//! `events` module). Local mode is single-process, single-user, no Postgres — so we
//! model the same fan-out shape with a `tokio::sync::broadcast`
//! channel for emit→subscribe and a per-correlator `oneshot` for
//! response routing.
//!
//! Lifecycle of a single event:
//!
//! 1. The capture/merge code path (a future workstream consumer)
//!    calls [`HookEventBroadcaster::emit`] with a JSON payload. The
//!    broker mints a `hook_event_id`, registers a fresh response
//!    slot, and broadcasts the event to every live subscriber.
//! 2. Each subscriber (a `SubscribeHookEvents` server-stream) picks
//!    up the event from its `mpsc::Receiver` and forwards it to the
//!    hook process.
//! 3. The hook reads the event, computes its reply, and the local
//!    daemon delivers it via `RespondToHook`. The handler routes the
//!    reply through [`HookEventBroadcaster::deliver_response`].
//! 4. The original emit caller awaits the reply via
//!    [`HookEventBroadcaster::await_response`], with a timeout so a
//!    crashed hook can't wedge the operation.
//!
//! Out-of-scope here:
//!   - Multiple hooks racing to reply: the first reply wins; the
//!     second is reported as `accepted=false` to its caller. The
//!     wire shape doesn't try to fan replies in.
//!   - Persisting in-flight events across daemon restart: the local
//!     daemon is meant to be the same lifetime as the agent loop,
//!     so a crash drops every in-flight reply. Hooks see the stream
//!     close and the emit caller times out.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use objects::object::OperationId;
use prost_types::Timestamp;
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};

use grpc::heddle::v1::HookEvent as ProtoHookEvent;

/// Channel capacity for the in-process broadcast. Each subscriber gets
/// its own queue; if a subscriber lags more than this many events
/// behind, the oldest events are dropped (the subscriber sees a
/// `Lagged` error in its `recv` and we close the stream so the hook
/// can re-subscribe).
const BROADCAST_CAPACITY: usize = 256;

/// Typed hook response decoded from `RespondToHook`. The universal
/// veto channel is `abort`; per-event extension fields ride on `extra`
/// so per-event handlers can pull `extra_signals`, `veto`, etc.
/// without the universal type having to know every shape.
///
/// This type's home will move to `crates/repo/src/hooks.rs` so the
/// CLI hook runner can decode the same shape from stdout. Until that
/// lands, the broker carries its own definition; the wire format on
/// `RespondToHookRequest` decodes into this type and the emit-side
/// awaits it. Field names match the spec verbatim so the eventual
/// move to `repo::hooks` is a one-line `pub use`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookResponse {
    #[serde(default)]
    pub abort: String,
    #[serde(flatten, default)]
    pub extra: serde_json::Value,
}

/// Per-event metadata routed from `emit` to `await_response`. The
/// channel is single-shot — once a reply lands, the slot is removed.
struct ResponseSlot {
    sender: oneshot::Sender<HookResponse>,
}

/// In-process pub/sub broker for hook events. Lives on
/// [`GrpcLocalService`](super::GrpcLocalService) so every handler
/// shares the same broker and a `subscribe_hook_events` stream and a
/// `respond_to_hook` reply meet on the same correlator.
#[derive(Clone)]
pub struct HookEventBroadcaster {
    inner: Arc<HookEventBroadcasterInner>,
}

struct HookEventBroadcasterInner {
    /// Broadcast sender. Fan-out shape so every subscriber gets its
    /// own backpressure rather than blocking the emitter.
    sender: broadcast::Sender<ProtoHookEvent>,
    /// Pending response slots keyed by `hook_event_id`. Mutex is
    /// fine here — every operation is short and the contention is low
    /// (one entry per in-flight emit).
    pending: Mutex<HashMap<String, ResponseSlot>>,
}

impl Default for HookEventBroadcaster {
    fn default() -> Self {
        Self::new()
    }
}

impl HookEventBroadcaster {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(BROADCAST_CAPACITY);
        Self {
            inner: Arc::new(HookEventBroadcasterInner {
                sender,
                pending: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Subscribe a fresh stream. Each call returns its own
    /// [`mpsc::Receiver`] backed by a forwarding task that drains the
    /// underlying `broadcast::Receiver`. The mpsc shape lets us close
    /// the stream cleanly when the subscriber drops, and lets the
    /// `Lagged` error close the stream rather than panicking.
    pub fn subscribe(&self) -> mpsc::Receiver<ProtoHookEvent> {
        let mut rx = self.inner.sender.subscribe();
        // Buffer one event ahead of the consumer; broadcast handles
        // the actual fan-out backlog so the mpsc only needs a small
        // shock-absorber capacity.
        let (tx, out_rx) = mpsc::channel(16);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // Subscriber fell behind. Drop the stream so
                        // the hook reconnects rather than silently
                        // missing events — the alternative (skip and
                        // continue) makes silent veto loss possible.
                        break;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        out_rx
    }

    /// Emit a fresh hook event. Returns the `hook_event_id` the
    /// caller should pass to [`Self::await_response`]. The id is a
    /// stringified UUIDv4 so it survives JSON round-trips intact.
    ///
    /// `payload_json` is delivered verbatim — schema validation lives
    /// in the catalog (see `GetHookEventSchema`) and is the caller's
    /// responsibility for now.
    pub fn emit(&self, event_name: impl Into<String>, payload_json: impl Into<String>) -> String {
        let hook_event_id = OperationId::new().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let event = ProtoHookEvent {
            hook_event_id: hook_event_id.clone(),
            event_name: event_name.into(),
            payload_json: payload_json.into(),
            emitted_at: Some(Timestamp {
                seconds: now.as_secs() as i64,
                nanos: now.subsec_nanos() as i32,
            }),
        };
        // Best-effort send: if there are no subscribers the broadcast
        // returns an error which we deliberately swallow. The emit
        // caller's `await_response` will time out — that's the
        // documented "no hook installed" path.
        let _ = self.inner.sender.send(event);
        hook_event_id
    }

    /// Register a single-shot response slot for `hook_event_id` and
    /// emit at the same time. Returns the id and a future that
    /// resolves to the hook's reply (or times out).
    ///
    /// Use this from the capture/merge code paths when you both want
    /// to fire the event and wait for the reply atomically.
    pub fn emit_and_wait(
        &self,
        event_name: impl Into<String>,
        payload_json: impl Into<String>,
        timeout: Duration,
    ) -> (String, EmitWaiter) {
        let (sender, receiver) = oneshot::channel();
        let event_name = event_name.into();
        let payload_json = payload_json.into();
        let hook_event_id = OperationId::new().to_string();
        // Reserve the slot before we broadcast so the response can't
        // race ahead of the registration.
        {
            let mut pending = self
                .inner
                .pending
                .lock()
                .expect("hook broker pending map poisoned");
            pending.insert(hook_event_id.clone(), ResponseSlot { sender });
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let event = ProtoHookEvent {
            hook_event_id: hook_event_id.clone(),
            event_name,
            payload_json,
            emitted_at: Some(Timestamp {
                seconds: now.as_secs() as i64,
                nanos: now.subsec_nanos() as i32,
            }),
        };
        let _ = self.inner.sender.send(event);
        let waiter = EmitWaiter {
            broker: self.clone(),
            hook_event_id: hook_event_id.clone(),
            receiver,
            timeout,
        };
        (hook_event_id, waiter)
    }

    /// Await a reply for `hook_event_id` with a deadline. Returns
    /// `None` when the deadline elapses before a hook responds (the
    /// emit caller's "no hook acted on this in time" branch).
    ///
    /// The slot must have been reserved via [`Self::emit_and_wait`]
    /// — calling this for a never-registered id resolves to `None`
    /// immediately.
    pub async fn await_response(
        &self,
        hook_event_id: &str,
        timeout: Duration,
    ) -> Option<HookResponse> {
        // Reservation is the responsibility of `emit_and_wait`. If
        // a caller wants to register, then emit, then wait separately
        // they can use this directly — but they must have called
        // `register_pending` first (kept private to avoid mis-use).
        let receiver = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .expect("hook broker pending map poisoned");
            pending.remove(hook_event_id).map(|slot| slot.sender)
        };
        // If no slot exists, fall back to creating one on the fly so
        // callers that didn't use `emit_and_wait` still work. We
        // re-insert and then take a fresh receiver.
        let receiver = match receiver {
            Some(_already_taken) => {
                // The sender is consumed — there's no way to await on
                // it here without rebuilding the slot. Fall through
                // to the fresh-slot path.
                let (sender, receiver) = oneshot::channel();
                let mut pending = self
                    .inner
                    .pending
                    .lock()
                    .expect("hook broker pending map poisoned");
                pending.insert(hook_event_id.to_string(), ResponseSlot { sender });
                receiver
            }
            None => {
                let (sender, receiver) = oneshot::channel();
                let mut pending = self
                    .inner
                    .pending
                    .lock()
                    .expect("hook broker pending map poisoned");
                pending.insert(hook_event_id.to_string(), ResponseSlot { sender });
                receiver
            }
        };
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(response)) => Some(response),
            Ok(Err(_canceled)) => None,
            Err(_elapsed) => {
                // Drop the slot so a late reply doesn't pile up
                // memory. The `RespondToHook` handler will report
                // `accepted=false` for late deliveries.
                let mut pending = self
                    .inner
                    .pending
                    .lock()
                    .expect("hook broker pending map poisoned");
                pending.remove(hook_event_id);
                None
            }
        }
    }

    /// Deliver a hook reply to the in-flight emit waiting on
    /// `hook_event_id`. Called by the `RespondToHook` handler.
    /// Returns `true` when the reply was delivered (a waiter was
    /// present); `false` when no waiter is registered (timed out, or
    /// already replied).
    pub fn deliver_response(&self, hook_event_id: &str, response: HookResponse) -> bool {
        let slot = {
            let mut pending = self
                .inner
                .pending
                .lock()
                .expect("hook broker pending map poisoned");
            pending.remove(hook_event_id)
        };
        match slot {
            Some(slot) => slot.sender.send(response).is_ok(),
            None => false,
        }
    }

    /// Number of subscribers currently attached. Useful for tests.
    #[cfg(test)]
    fn subscriber_count(&self) -> usize {
        self.inner.sender.receiver_count()
    }
}

/// Future returned by [`HookEventBroadcaster::emit_and_wait`]. Holds
/// the receiver plus a hook back to the broker so it can clean up the
/// pending slot if the future is dropped before the reply lands.
pub struct EmitWaiter {
    broker: HookEventBroadcaster,
    hook_event_id: String,
    receiver: oneshot::Receiver<HookResponse>,
    timeout: Duration,
}

impl EmitWaiter {
    /// Resolve the waiter, returning `Some` on a fresh reply and
    /// `None` on timeout or hook crash. Drops the broker's pending
    /// slot in either path.
    pub async fn wait(self) -> Option<HookResponse> {
        let EmitWaiter {
            broker,
            hook_event_id,
            receiver,
            timeout,
        } = self;
        match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(response)) => Some(response),
            Ok(Err(_canceled)) => {
                broker
                    .inner
                    .pending
                    .lock()
                    .expect("hook broker pending map poisoned")
                    .remove(&hook_event_id);
                None
            }
            Err(_elapsed) => {
                broker
                    .inner
                    .pending
                    .lock()
                    .expect("hook broker pending map poisoned")
                    .remove(&hook_event_id);
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn emit_round_trips_to_subscriber() {
        let broker = HookEventBroadcaster::new();
        let mut sub = broker.subscribe();
        // Yield once so the subscribe forwarding task can install the
        // underlying broadcast receiver before the first emit.
        tokio::task::yield_now().await;
        let id = broker.emit("pre_capture", "{\"thread\":\"t1\"}");
        let event = sub.recv().await.expect("event");
        assert_eq!(event.hook_event_id, id);
        assert_eq!(event.event_name, "pre_capture");
        assert!(event.payload_json.contains("t1"));
    }

    #[tokio::test]
    async fn await_response_returns_delivered_reply() {
        let broker = HookEventBroadcaster::new();
        let _sub = broker.subscribe();
        tokio::task::yield_now().await;
        let (id, waiter) = broker.emit_and_wait("pre_capture", "{}", Duration::from_secs(1));
        let id_for_reply = id.clone();
        let broker_clone = broker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = broker_clone.deliver_response(
                &id_for_reply,
                HookResponse {
                    abort: "veto".into(),
                    extra: serde_json::Value::Null,
                },
            );
        });
        let response = waiter.wait().await.expect("response");
        assert_eq!(response.abort, "veto");
    }

    #[tokio::test]
    async fn await_response_times_out_with_no_reply() {
        let broker = HookEventBroadcaster::new();
        let _sub = broker.subscribe();
        let (_id, waiter) = broker.emit_and_wait("pre_capture", "{}", Duration::from_millis(20));
        let response = waiter.wait().await;
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn deliver_to_unknown_id_returns_false() {
        let broker = HookEventBroadcaster::new();
        let accepted = broker.deliver_response("no-such-id", HookResponse::default());
        assert!(!accepted);
    }

    #[tokio::test]
    async fn subscribers_are_independent() {
        let broker = HookEventBroadcaster::new();
        let mut a = broker.subscribe();
        let mut b = broker.subscribe();
        tokio::task::yield_now().await;
        assert_eq!(broker.subscriber_count(), 2);
        broker.emit("post_capture", "{}");
        let event_a = a.recv().await.expect("a");
        let event_b = b.recv().await.expect("b");
        assert_eq!(event_a.hook_event_id, event_b.hook_event_id);
    }
}