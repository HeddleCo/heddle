#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Poll, Waker},
        time::{Duration, Instant},
    };

    use objects::{
        error::Result,
        object::{Attribution, Blob, ContentHash, Principal, State, StateId, Tree},
        store::AsyncObjectSource,
        transfer::{AncestryBudget, AncestryError, is_ancestor_async_bounded},
    };

    struct CancelOnFinalRead(Arc<AtomicBool>);
    impl AsyncObjectSource for CancelOnFinalRead {
        async fn get_tree(&self, _: &ContentHash) -> Result<Option<Tree>> {
            Ok(None)
        }
        async fn get_blob(&self, _: &ContentHash) -> Result<Option<Blob>> {
            Ok(None)
        }
        async fn get_state(&self, _: &StateId) -> Result<Option<State>> {
            self.0.store(true, Ordering::Release);
            Ok(None)
        }
    }
    #[test]
    fn cancellation_during_final_read_must_not_become_non_ancestry() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let source = CancelOnFinalRead(cancelled.clone());
        let budget = AncestryBudget {
            max_states: 4,
            cancelled,
            deadline: None,
        };
        let ancestor = StateId::from_bytes([1; 32]);
        let descendant = StateId::from_bytes([2; 32]);
        let mut future = Box::pin(is_ancestor_async_bounded(
            &source,
            &ancestor,
            &descendant,
            &budget,
        ));
        let mut cx = Context::from_waker(Waker::noop());
        let result = match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("fixture reads are immediate"),
        };
        assert!(
            matches!(result, Err(AncestryError::Cancelled)),
            "cancellation became {result:?}"
        );
    }

    fn state(parents: Vec<StateId>) -> State {
        State::new(
            Tree::new().hash(),
            parents,
            Attribution::human(Principal::new("test", "test@example.com")),
        )
    }

    fn run<T>(future: impl Future<Output = T>) -> T {
        let mut future = Box::pin(future);
        let mut context = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("fixture reads are immediate"),
        }
    }

    struct FinalReadSource {
        final_read: StateId,
        final_state: Option<State>,
        preceding_state: Option<State>,
        cancelled: Option<Arc<AtomicBool>>,
        deadline: Option<Instant>,
    }

    impl AsyncObjectSource for FinalReadSource {
        async fn get_tree(&self, _: &ContentHash) -> Result<Option<Tree>> {
            Ok(None)
        }

        async fn get_blob(&self, _: &ContentHash) -> Result<Option<Blob>> {
            Ok(None)
        }

        async fn get_state(&self, id: &StateId) -> Result<Option<State>> {
            if *id != self.final_read {
                return Ok(self.preceding_state.clone());
            }
            if let Some(deadline) = self.deadline {
                while Instant::now() < deadline {
                    std::hint::spin_loop();
                }
            }
            if let Some(cancelled) = &self.cancelled {
                cancelled.store(true, Ordering::Release);
            }
            Ok(self.final_state.clone())
        }
    }

    #[test]
    fn cancellation_during_terminal_non_matching_read_wins() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let ancestor = state(vec![]).id();
        let terminal = state(vec![]);
        let descendant = terminal.id();
        let source = FinalReadSource {
            final_read: descendant,
            final_state: Some(terminal),
            preceding_state: None,
            cancelled: Some(cancelled.clone()),
            deadline: None,
        };
        let budget = AncestryBudget {
            max_states: 4,
            cancelled,
            deadline: None,
        };

        let result = run(is_ancestor_async_bounded(
            &source,
            &ancestor,
            &descendant,
            &budget,
        ));

        assert!(
            matches!(result, Err(AncestryError::Cancelled)),
            "cancellation became {result:?}"
        );
    }

    #[test]
    fn cancellation_during_matching_ancestor_read_wins() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let ancestor_state = state(vec![]);
        let ancestor = ancestor_state.id();
        let descendant_state = state(vec![ancestor]);
        let descendant = descendant_state.id();
        let source = FinalReadSource {
            final_read: ancestor,
            final_state: Some(ancestor_state),
            preceding_state: Some(descendant_state),
            cancelled: Some(cancelled.clone()),
            deadline: None,
        };
        let budget = AncestryBudget {
            max_states: 4,
            cancelled,
            deadline: None,
        };

        let result = run(is_ancestor_async_bounded(
            &source,
            &ancestor,
            &descendant,
            &budget,
        ));

        assert!(
            matches!(result, Err(AncestryError::Cancelled)),
            "cancellation became {result:?}"
        );
    }

    #[test]
    fn deadline_expiry_during_final_read_wins() {
        let deadline = Instant::now() + Duration::from_millis(10);
        let ancestor = StateId::from_bytes([1; 32]);
        let descendant = StateId::from_bytes([2; 32]);
        let source = FinalReadSource {
            final_read: descendant,
            final_state: None,
            preceding_state: None,
            cancelled: None,
            deadline: Some(deadline),
        };
        let budget = AncestryBudget {
            max_states: 4,
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: Some(deadline),
        };

        let result = run(is_ancestor_async_bounded(
            &source,
            &ancestor,
            &descendant,
            &budget,
        ));

        assert!(
            matches!(result, Err(AncestryError::Deadline)),
            "deadline expiry became {result:?}"
        );
    }

    #[test]
    fn valid_budget_preserves_missing_and_matching_semantics() {
        let ancestor = StateId::from_bytes([1; 32]);
        let descendant = StateId::from_bytes([2; 32]);
        let budget = AncestryBudget {
            max_states: 4,
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline: None,
        };
        let missing_source = FinalReadSource {
            final_read: descendant,
            final_state: None,
            preceding_state: None,
            cancelled: None,
            deadline: None,
        };
        let missing_result = run(is_ancestor_async_bounded(
            &missing_source,
            &ancestor,
            &descendant,
            &budget,
        ));
        assert!(
            matches!(missing_result, Ok(false)),
            "missing parent became {missing_result:?}"
        );

        let ancestor_state = state(vec![]);
        let ancestor = ancestor_state.id();
        let descendant_state = state(vec![ancestor]);
        let descendant = descendant_state.id();
        let matching_source = FinalReadSource {
            final_read: ancestor,
            final_state: Some(ancestor_state),
            preceding_state: Some(descendant_state),
            cancelled: None,
            deadline: None,
        };
        let matching_result = run(is_ancestor_async_bounded(
            &matching_source,
            &ancestor,
            &descendant,
            &budget,
        ));
        assert!(
            matches!(matching_result, Ok(true)),
            "matching ancestor became {matching_result:?}"
        );
    }
}
