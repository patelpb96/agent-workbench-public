//! Private bounded fake decisions and delivery accounting; no producer channel.
use super::*;

const SUCCESS_TEXT: [&str; 3] = [
    "SIMULATED: response from the local fake provider.\n",
    "SIMULATED: this bounded chunk contains display text only.\n",
    "SIMULATED: fake response complete.\n",
];
const WAIT_TEXT: [&str; 2] = [
    "SIMULATED: waiting scenario started.\n",
    "SIMULATED: awaiting a persisted cancellation request.\n",
];

pub(super) fn decide(state: RunState, scenario: FakeScenario, emitted: u8) -> Option<EventPayload> {
    let transition = |to, error| {
        EventPayload::RunStateChanged(RunStateChangedPayload {
            from: state,
            to,
            error,
        })
    };
    match state {
        RunState::Queued => Some(transition(RunState::Starting, None)),
        RunState::CancelRequested => Some(transition(RunState::Cancelled, None)),
        RunState::Starting => Some(if scenario == FakeScenario::Failure {
            transition(
                RunState::Failed,
                Some(v::error(
                    "fake_start_failed",
                    "SIMULATED startup failure; no child was launched.",
                )),
            )
        } else {
            transition(RunState::Running, None)
        }),
        RunState::Running => {
            let text = match scenario {
                FakeScenario::Success => SUCCESS_TEXT.get(emitted as usize),
                FakeScenario::WaitForCancel => WAIT_TEXT.get(emitted as usize),
                FakeScenario::Failure => None,
            };
            if let Some(text) = text {
                return Some(EventPayload::TextDelta(TextDeltaPayload {
                    stream: TextStream::Assistant,
                    text: (*text).into(),
                }));
            }
            match scenario {
                FakeScenario::Success if emitted == SUCCESS_TEXT.len() as u8 => {
                    Some(EventPayload::UsageUpdated(Usage {
                        input_tokens: None,
                        output_tokens: None,
                        estimated: false,
                    }))
                }
                FakeScenario::Success => Some(transition(RunState::Succeeded, None)),
                _ => None,
            }
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct BudgetLimits {
    pub(super) pending_bytes: usize,
    pub(super) text_bytes: usize,
    pub(super) ordinary_events: usize,
    pub(super) reserved_events: usize,
    pub(super) control_bytes: usize,
}

impl BudgetLimits {
    pub(super) fn production() -> Self {
        Self {
            pending_bytes: PENDING_BYTES,
            text_bytes: PERSISTED_TEXT_BYTES,
            ordinary_events: ORDINARY_EVENTS,
            reserved_events: RESERVED_EVENTS,
            control_bytes: CONTROL_PENDING_BYTES,
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Budget {
    pub(super) ordinary: usize,
    pub(super) text_bytes: usize,
    pub(super) reserved: usize,
    pub(super) markers: [bool; 3],
    pub(super) cancel_intent: bool,
    pub(super) terminal: bool,
    pub(super) stopped: bool,
}

#[derive(Debug)]
pub(super) struct Discard {
    pub(super) scopes: [bool; 3],
    pub(super) events: usize,
    pub(super) encoded_bytes: usize,
    pub(super) text_bytes: usize,
}

#[derive(Debug)]
pub(super) enum BudgetCheck {
    Accept(Budget),
    Discard(Discard),
}

fn scope_index(scope: TruncationScope) -> usize {
    match scope {
        TruncationScope::PendingBytes => 0,
        TruncationScope::PersistedTextBytes => 1,
        TruncationScope::EventCount => 2,
    }
}

impl Budget {
    pub(super) fn fresh() -> Self {
        // Admission has already committed exactly one ordinary creation event.
        // Preexisting active flows are recovered, never assigned this budget.
        Self {
            ordinary: 1,
            text_bytes: 0,
            reserved: 0,
            markers: [false; 3],
            cancel_intent: false,
            terminal: false,
            stopped: false,
        }
    }

    pub(super) fn check(
        &self,
        events: &[RunEvent],
        limits: BudgetLimits,
    ) -> StoreResult<BudgetCheck> {
        if events.is_empty() || events.len() > PAGE_EVENTS {
            return Err(v::error(
                "validation_failed",
                "Invalid bounded delivery size.",
            ));
        }
        let mut next = self.clone();
        let mut ordinary_events = 0usize;
        let mut ordinary_bytes = 0usize;
        let mut text_bytes = 0usize;
        let mut control_bytes = 0usize;
        for event in events {
            // Check UTF-8 length before allocating its encoded representation.
            if let RunEvent::TextDelta { payload, .. } = event {
                if payload.text.is_empty() || payload.text.len() > TEXT_DELTA_BYTES {
                    return Err(v::error(
                        "validation_failed",
                        "Text delta exceeds its byte budget.",
                    ));
                }
            }
            v::validate_event(event)?;
            let bytes = serde_json::to_vec(event)
                .map_err(|_| v::error("validation_failed", "Cannot encode bounded delivery."))?
                .len();
            if v::is_reserved_event(event) {
                next.reserved = next.reserved.saturating_add(1);
                control_bytes = control_bytes.saturating_add(bytes);
                match event {
                    RunEvent::OutputTruncated { payload, .. } => {
                        let index = scope_index(payload.scope);
                        if next.markers[index] {
                            return Err(unavailable("Duplicate truncation marker."));
                        }
                        next.markers[index] = true;
                        next.stopped = true;
                    }
                    RunEvent::RunStateChanged { payload, .. }
                        if payload.to == RunState::CancelRequested =>
                    {
                        if next.cancel_intent {
                            return Err(unavailable("Duplicate persisted stop intent."));
                        }
                        next.cancel_intent = true;
                        next.stopped = true;
                    }
                    RunEvent::RunStateChanged { payload, .. } if v::is_terminal(payload.to) => {
                        if next.terminal {
                            return Err(unavailable("Duplicate terminal control event."));
                        }
                        next.terminal = true;
                        next.stopped = true;
                    }
                    RunEvent::RunRecovered { .. } => {
                        if next.terminal {
                            return Err(unavailable("Duplicate recovery control event."));
                        }
                        next.terminal = true;
                        next.stopped = true;
                    }
                    _ => {}
                }
            } else {
                if next.stopped {
                    return Err(v::error(
                        "state_conflict",
                        "Ordinary production has stopped.",
                    ));
                }
                ordinary_events = ordinary_events.saturating_add(1);
                ordinary_bytes = ordinary_bytes.saturating_add(bytes);
                if let RunEvent::TextDelta { payload, .. } = event {
                    text_bytes = text_bytes.saturating_add(payload.text.len());
                }
            }
        }
        if next.reserved > limits.reserved_events || control_bytes > limits.control_bytes {
            return Err(unavailable("Reserved control capacity exhausted."));
        }
        let scopes = [
            ordinary_bytes > limits.pending_bytes,
            text_bytes > 0 && self.text_bytes.saturating_add(text_bytes) > limits.text_bytes,
            ordinary_events > 0
                && self.ordinary.saturating_add(ordinary_events) > limits.ordinary_events,
        ];
        if scopes.into_iter().any(|overflow| overflow) {
            return Ok(BudgetCheck::Discard(Discard {
                scopes,
                events: ordinary_events,
                encoded_bytes: ordinary_bytes,
                text_bytes,
            }));
        }
        next.ordinary = next.ordinary.saturating_add(ordinary_events);
        next.text_bytes = next.text_bytes.saturating_add(text_bytes);
        Ok(BudgetCheck::Accept(next))
    }
}
