//! One awaitable reply per admitted request; no blocking executor receives.
use crate::execution::ports::StoreResult;
use serde_json::Value;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll, Waker},
};

// A slot remains owned until BOTH execution and caller/reply storage are done.
// Dropping a caller never frees an unexecuted job's slot or cancels its mutation.
pub(super) struct Permit(Arc<AtomicUsize>);
impl Permit {
    pub fn acquire(counter: &Arc<AtomicUsize>, limit: usize) -> StoreResult<Arc<Self>> {
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < limit).then_some(count + 1)
            })
            .map_err(|_| super::coordinator::busy())?;
        Ok(Arc::new(Self(counter.clone())))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}
struct State {
    result: Option<StoreResult<Value>>,
    waker: Option<Waker>,
    abandoned: bool,
}
struct Shared(Mutex<State>);

/// Await after nonblocking admission. It owns no runtime/database access.
pub struct Reply {
    shared: Arc<Shared>,
    _permit: Arc<Permit>,
}
pub(super) struct Sender {
    shared: Arc<Shared>,
    _permit: Arc<Permit>,
    completed: bool,
}
pub(super) fn pair(permit: Arc<Permit>) -> (Sender, Reply) {
    let shared = Arc::new(Shared(Mutex::new(State {
        result: None,
        waker: None,
        abandoned: false,
    })));
    (
        Sender {
            shared: shared.clone(),
            _permit: permit.clone(),
            completed: false,
        },
        Reply {
            shared,
            _permit: permit,
        },
    )
}
impl Sender {
    pub fn complete(mut self, result: StoreResult<Value>) {
        self.completed = true;
        let waker = {
            let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
            if !state.abandoned {
                state.result = Some(result);
            }
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        if self.completed {
            return;
        }
        let waker = {
            let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
            if !state.abandoned && state.result.is_none() {
                state.result = Some(Err(super::coordinator::unavailable()));
            }
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}
impl Future for Reply {
    type Output = StoreResult<Value>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(result) = state.result.take() {
            return Poll::Ready(result);
        }
        state.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}
impl Drop for Reply {
    fn drop(&mut self) {
        let mut state = self.shared.0.lock().unwrap_or_else(|e| e.into_inner());
        state.abandoned = true;
        state.result = None;
        state.waker = None;
    }
}
