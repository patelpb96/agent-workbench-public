//! C28 v2 writer tests on the nonce-owned COPY: append, cancel, layout, recovery.
//! Native fixtures only; no provider process, dispatch arm or original write.
use super::admission_v2::{codex, create, digest, fake, family, rehearsal};
use super::*;
use crate::execution_v2::{
    self as v2,
    admission::StartRequest,
    ports::{AdmissionJournal, AppendBatch, JournalReader, JournalWriter},
    validation as policy,
};
use crate::storage::admission_v2::AdmissionRehearsal;

fn source(run: &v2::RunSnapshot, payload: &v2::EventPayload) -> v2::EventSource {
    match payload {
        v2::EventPayload::TextDelta(_) | v2::EventPayload::UsageUpdated(_)
            if run.provider_id == v2::ProviderId::Codex =>
        {
            v2::EventSource::Provider
        }
        v2::EventPayload::TextDelta(_) | v2::EventPayload::ToolActivity(_) => {
            v2::EventSource::Fixture
        }
        _ => v2::EventSource::System,
    }
}
fn event_at(
    run: &v2::RunSnapshot,
    payload: v2::EventPayload,
    src: v2::EventSource,
) -> v2::RunEvent {
    v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: id(),
            workspace_id: run.workspace_id.clone(),
            session_id: run.session_id.clone(),
            run_id: run.run_id.clone(),
            provider_id: run.provider_id,
            source: src,
            seq: run.last_seq + 1,
            occurred_at: LATER.into(),
        },
        payload,
    )
}
pub(super) fn batch(run: &v2::RunSnapshot, payloads: Vec<v2::EventPayload>) -> AppendBatch {
    let mut next = run.clone();
    let mut events = Vec::new();
    for payload in payloads {
        let src = source(&next, &payload);
        let event = event_at(&next, payload, src);
        next = policy::apply_event(&next, &event).unwrap();
        events.push(event);
    }
    AppendBatch {
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        expected_last_seq: run.last_seq,
        expected_state: run.state,
        events,
        next_snapshot: next,
    }
}
/// Proposal built WITHOUT the reducer, so only the store can refuse it.
fn raw(run: &v2::RunSnapshot, event: v2::RunEvent, state: v2::RunState) -> AppendBatch {
    let mut next = run.clone();
    next.state = state;
    next.last_seq += 1;
    next.updated_at = LATER.into();
    AppendBatch {
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        expected_last_seq: run.last_seq,
        expected_state: run.state,
        events: vec![event],
        next_snapshot: next,
    }
}
fn change(from: v2::RunState, to: v2::RunState) -> v2::EventPayload {
    v2::EventPayload::RunStateChanged(v2::RunStateChangedPayload {
        from,
        to,
        error: None,
    })
}
pub(super) fn to(run: &v2::RunSnapshot, state: v2::RunState) -> AppendBatch {
    batch(run, vec![change(run.state, state)])
}
pub(super) fn text(body: &str) -> v2::EventPayload {
    v2::EventPayload::TextDelta(v2::TextDeltaPayload {
        stream: v2::TextStream::Assistant,
        text: body.into(),
    })
}
pub(super) fn reported() -> v2::EventPayload {
    v2::EventPayload::UsageUpdated(v2::Usage {
        input_tokens: Some(12),
        output_tokens: Some(3),
        estimated: false,
        cached_input_tokens: Some(0),
        cache_write_input_tokens: None,
        reasoning_output_tokens: Some(0),
    })
}
fn codex_run(j: &mut AdmissionRehearsal, w: &str) -> v2::RunSnapshot {
    let s = j
        .create_session(&create(w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    j.enqueue_run(
        StartRequest::CodexText(&codex(w, &s.session_id)),
        run_seed(),
    )
    .unwrap()
    .result
}
fn fake_run(j: &mut AdmissionRehearsal, w: &str, session: &str) -> v2::RunSnapshot {
    j.enqueue_run(StartRequest::Fake(&fake(w, session)), run_seed())
        .unwrap()
        .result
}
fn running(j: &mut AdmissionRehearsal, run: v2::RunSnapshot) -> v2::RunSnapshot {
    let run = j
        .append_batch(to(&run, v2::RunState::Starting))
        .unwrap()
        .result;
    j.append_batch(to(&run, v2::RunState::Running))
        .unwrap()
        .result
}
fn cancel_request(run: &v2::RunSnapshot) -> v2::CancelRunRequest {
    v2::CancelRunRequest {
        contract_version: 2,
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        request_id: id(),
    }
}
fn counters(j: &mut AdmissionRehearsal, run: &str) -> [u64; 6] {
    j.connection().query_row("SELECT ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count FROM runs WHERE run_id=?1",
        [run], |r| Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?])).unwrap()
}
fn page(run: &v2::RunSnapshot) -> v2::EventPageRequest {
    v2::EventPageRequest {
        contract_version: 2,
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        after_seq: 0,
        through_seq: None,
        limit: 200,
    }
}

#[test]
fn codex_run_appends_to_success_with_reads_audit_and_originals_intact() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let original = family(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let run = running(&mut j, queued_first);
    // Multi-event batch: Unicode text chunks, then complete reported usage.
    let saved = j
        .append_batch(batch(
            &run,
            vec![text("Hello 😀 "), text("world\n"), reported()],
        ))
        .unwrap();
    assert!(!saved.replayed && saved.committed_events.len() == 3);
    let done = j
        .append_batch(to(&saved.result, v2::RunState::Succeeded))
        .unwrap()
        .result;
    assert_eq!(done.state, v2::RunState::Succeeded);
    assert_eq!(done.last_seq, 7);
    assert_eq!(done.usage.input_tokens, Some(12));
    assert_eq!(
        counters(&mut j, &done.run_id),
        [6, 1, "Hello 😀 world\n".len() as u64, 0, 0, 1]
    );
    // Direct reads and the full copy audit agree with the derived snapshot.
    assert_eq!(j.get_run(&w, &done.run_id).unwrap().snapshot, done);
    let events = j.list_run_events(&page(&done)).unwrap().events;
    assert_eq!(events.len(), 7);
    assert!(events
        .iter()
        .enumerate()
        .all(|(i, e)| e.header().seq == i as u64 + 1));
    let audited = j.audit_checkpoint().unwrap();
    assert!(audited.runs.contains(&done));
    assert!(!audited.execution_available);
    // The live copy is not the original family; the original stays byte-identical.
    assert_eq!(family(&directory.0), original);
    j.close().unwrap();
    assert_eq!(family(&directory.0), original);
}

#[test]
fn stale_preconditions_identity_profile_and_legacy_history_fail_without_writes() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let run = running(&mut j, queued_first);
    let before = digest(j.connection());
    let code = |j: &mut AdmissionRehearsal, b: AppendBatch| j.append_batch(b).err().unwrap().code;

    let mut stale = batch(&run, vec![text("x")]);
    stale.expected_last_seq -= 1;
    assert_eq!(code(&mut j, stale), "state_conflict");
    let mut stale = batch(&run, vec![text("x")]);
    stale.expected_state = v2::RunState::Starting;
    assert_eq!(code(&mut j, stale), "state_conflict");
    // Global event identity: reuse the committed run_created event ID.
    let created = j.list_run_events(&page(&run)).unwrap().events.remove(0);
    let mut dup = batch(&run, vec![text("x")]);
    let payload = text("x");
    let mut h = dup.events[0].header();
    h.event_id = created.header().event_id;
    dup.events[0] = v2::RunEvent::from_parts(h, payload);
    assert_eq!(code(&mut j, dup), "state_conflict");
    // Caller-asserted snapshot must equal the derived one.
    let mut forged = batch(&run, vec![text("x")]);
    forged.next_snapshot.updated_at = NOW.into();
    assert!(j.append_batch(forged).is_err());
    // Codex profile policy: no tools, no system-sourced text, no oversize chunk,
    // no waiting_input, and success requires complete reported usage.
    let tool = v2::EventPayload::ToolActivity(v2::ToolActivityPayload {
        tool_call_id: "t".into(),
        title: "t".into(),
        status: v2::ToolStatus::Started,
    });
    let bad_events = [
        event_at(&run, tool, v2::EventSource::Provider),
        event_at(&run, text("x"), v2::EventSource::System),
        event_at(
            &run,
            text(&"x".repeat(v2::CODEX_COMPLETION_CHUNK_BYTES + 1)),
            v2::EventSource::Provider,
        ),
        event_at(
            &run,
            change(run.state, v2::RunState::WaitingInput),
            v2::EventSource::System,
        ),
    ];
    for e in bad_events {
        let mut b = batch(&run, vec![text("x")]);
        b.events = vec![e];
        assert!(j.append_batch(b).is_err());
    }
    // Codex success without complete reported usage is refused by the store.
    let success = event_at(
        &run,
        change(run.state, v2::RunState::Succeeded),
        v2::EventSource::System,
    );
    assert!(j
        .append_batch(raw(&run, success, v2::RunState::Succeeded))
        .is_err());
    // Legacy (v1-original) archived history cannot acquire v2 events.
    let legacy = j
        .audit_checkpoint()
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.state == v2::RunState::Interrupted)
        .unwrap();
    let mut b = batch(&run, vec![text("x")]);
    b.run_id = legacy.run_id.clone();
    b.expected_last_seq = legacy.last_seq;
    b.expected_state = legacy.state;
    assert_eq!(code(&mut j, b), "state_conflict");
    // Unknown run and foreign workspace.
    let mut b = batch(&run, vec![text("x")]);
    b.run_id = id();
    assert_eq!(code(&mut j, b), "run_not_found");
    let mut b = batch(&run, vec![text("x")]);
    b.workspace_id = id();
    assert!(j.append_batch(b).is_err());
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
}

#[test]
fn one_active_slot_and_fifo_admission_hold_across_fake_and_codex() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let first = fake_run(&mut j, &w, &legacy);
    let second = codex_run(&mut j, &w);
    let before = digest(j.connection());
    // FIFO: the later queued run cannot be admitted first.
    assert_eq!(
        j.append_batch(to(&second, v2::RunState::Starting))
            .err()
            .unwrap()
            .code,
        "state_conflict"
    );
    assert_eq!(digest(j.connection()), before);
    let active = running(&mut j, first);
    // Slot: even a compound batch that ends terminal cannot claim it meanwhile.
    let compound = batch(
        &second,
        vec![
            change(v2::RunState::Queued, v2::RunState::Starting),
            change(v2::RunState::Starting, v2::RunState::Interrupted),
        ],
    );
    assert!(j.append_batch(compound).is_err());
    let done = j
        .append_batch(to(&active, v2::RunState::Succeeded))
        .unwrap()
        .result;
    assert_eq!(done.state, v2::RunState::Succeeded);
    running(&mut j, second);
    j.audit_checkpoint().unwrap();
}

#[test]
fn cumulative_text_budget_spans_transactions_then_truncation_stops_atomically() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let queued_first = fake_run(&mut j, &w, &legacy);
    let mut run = running(&mut j, queued_first);
    // 4096 four-byte scalars = TEXT_DELTA_BYTES UTF-8 bytes per chunk.
    let chunk = "😀".repeat(v2::TEXT_DELTA_BYTES / 4);
    assert_eq!(chunk.len(), v2::TEXT_DELTA_BYTES);
    let mut chunks = v2::PERSISTED_TEXT_BYTES / v2::TEXT_DELTA_BYTES;
    while chunks > 0 {
        let count = chunks.min(48); // encoded batch stays under PENDING_BYTES
        run = j
            .append_batch(batch(&run, (0..count).map(|_| text(&chunk)).collect()))
            .unwrap()
            .result;
        chunks -= count;
    }
    assert_eq!(
        counters(&mut j, &run.run_id)[2],
        v2::PERSISTED_TEXT_BYTES as u64
    );
    let before = digest(j.connection());
    // One more byte exceeds the cumulative budget: whole batch refused.
    assert_eq!(
        j.append_batch(batch(&run, vec![text("x")]))
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    // A single oversized batch is refused by the pending-bytes rule.
    assert!(j
        .append_batch(batch(&run, (0..70).map(|_| text(&chunk)).collect()))
        .is_err());
    assert_eq!(digest(j.connection()), before);
    // Truncation must stop in the same batch; marker alone is refused.
    let marker = || {
        v2::EventPayload::OutputTruncated(v2::OutputTruncatedPayload {
            scope: v2::TruncationScope::PersistedTextBytes,
            discarded_bytes: Some(1),
            discarded_events: 1,
        })
    };
    assert!(j.append_batch(batch(&run, vec![marker()])).is_err());
    let stopping = j
        .append_batch(batch(
            &run,
            vec![marker(), change(run.state, v2::RunState::CancelRequested)],
        ))
        .unwrap()
        .result;
    // No ordinary output, repeat scope or success after truncation.
    let before = digest(j.connection());
    let late = batch(&stopping, vec![text("x")]);
    assert_eq!(j.append_batch(late).err().unwrap().code, "state_conflict");
    assert!(j
        .append_batch(batch(
            &stopping,
            vec![marker(), change(stopping.state, v2::RunState::Cancelled)]
        ))
        .is_err());
    assert_eq!(digest(j.connection()), before);
    let done = j
        .append_batch(to(&stopping, v2::RunState::Cancelled))
        .unwrap()
        .result;
    let [ordinary, reserved, bytes, mask, cancels, terminals] = counters(&mut j, &done.run_id);
    assert_eq!(
        (ordinary, reserved, bytes, mask, cancels, terminals),
        (3 + 640, 3, v2::PERSISTED_TEXT_BYTES as u64, 2, 1, 1)
    );
    j.audit_checkpoint().unwrap();
}

#[test]
fn insert_update_and_commit_failures_roll_back_the_entire_append() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let run = running(&mut j, queued_first);
    let before = digest(j.connection());
    for trigger in [
        "CREATE TEMP TRIGGER fail_write BEFORE INSERT ON events BEGIN SELECT RAISE(ABORT,'injected'); END",
        "CREATE TEMP TRIGGER fail_write BEFORE UPDATE ON runs BEGIN SELECT RAISE(ABORT,'injected'); END",
    ] {
        j.connection().execute_batch(trigger).unwrap();
        assert!(j
            .append_batch(batch(&run, vec![text("a"), text("b")]))
            .is_err());
        assert_eq!(digest(j.connection()), before, "{trigger}");
        j.connection().execute_batch("DROP TRIGGER fail_write").unwrap();
    }
    j.connection().execute_batch("PRAGMA defer_foreign_keys=ON;
        CREATE TEMP TRIGGER fail_commit AFTER UPDATE ON runs BEGIN
        INSERT INTO sessions(session_id,workspace_id,snapshot_json) VALUES ('invalid-orphan','missing-workspace','{}'); END;").unwrap();
    assert!(j.append_batch(batch(&run, vec![text("a")])).is_err());
    assert_eq!(digest(j.connection()), before);
    j.connection()
        .execute_batch("DROP TRIGGER fail_commit")
        .unwrap();
    // The same proposal then succeeds: nothing partial was left behind.
    j.append_batch(batch(&run, vec![text("a"), text("b")]))
        .unwrap();
    j.audit_checkpoint().unwrap();
}

#[test]
fn cancellation_is_receipted_replayable_and_cancel_intent_defeats_success() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy_session) = rehearsal(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let active = running(&mut j, queued_first);
    let queued = fake_run(&mut j, &w, &legacy_session);
    // Queued: direct cancel, never starts.
    let request = cancel_request(&queued);
    let saved = j
        .cancel_run(&request, Some(to(&queued, v2::RunState::Cancelled)))
        .unwrap();
    assert_eq!(saved.result.state, v2::RunState::Cancelled);
    assert_eq!(saved.committed_events.len(), 1);
    let before = digest(j.connection());
    let replay = j.cancel_run(&request, None).unwrap();
    assert!(replay.replayed && replay.committed_events.is_empty());
    assert_eq!(replay.result, saved.result);
    assert_eq!(digest(j.connection()), before);
    // Active: persisted intent; success is then impossible.
    let wrong = to(&active, v2::RunState::Interrupted);
    assert_eq!(
        j.cancel_run(&cancel_request(&active), Some(wrong))
            .err()
            .unwrap()
            .code,
        "state_conflict"
    );
    assert!(j.cancel_run(&cancel_request(&active), None).is_err());
    let stopping = j
        .cancel_run(
            &cancel_request(&active),
            Some(to(&active, v2::RunState::CancelRequested)),
        )
        .unwrap()
        .result;
    assert_eq!(stopping.state, v2::RunState::CancelRequested);
    let success = event_at(
        &stopping,
        change(stopping.state, v2::RunState::Succeeded),
        v2::EventSource::System,
    );
    assert!(j
        .append_batch(raw(&stopping, success, v2::RunState::Succeeded))
        .is_err());
    // Same request identity with different arguments conflicts, writes nothing.
    let before = digest(j.connection());
    let mut changed = request.clone();
    changed.run_id = stopping.run_id.clone();
    assert_eq!(
        j.cancel_run(&changed, None).err().unwrap().code,
        "request_id_conflict"
    );
    assert_eq!(digest(j.connection()), before);
    // No-op cancels on cancel_requested, terminal v2 and archived v1 runs.
    assert!(j
        .cancel_run(
            &cancel_request(&stopping),
            Some(to(&stopping, v2::RunState::Cancelled))
        )
        .is_err());
    let noop = j.cancel_run(&cancel_request(&stopping), None).unwrap();
    assert!(!noop.replayed && noop.committed_events.is_empty() && noop.result == stopping);
    let archived = j
        .audit_checkpoint()
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.state == v2::RunState::Interrupted)
        .unwrap();
    assert_eq!(
        j.cancel_run(&cancel_request(&archived), None)
            .unwrap()
            .result,
        archived
    );
    let done = j
        .append_batch(to(&stopping, v2::RunState::Cancelled))
        .unwrap()
        .result;
    assert_eq!(
        j.cancel_run(&cancel_request(&done), None).unwrap().result,
        done
    );
    let emergency: u64 = j
        .connection()
        .query_row("SELECT COUNT(*) FROM receipts WHERE emergency=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(emergency, 0);
    j.audit_checkpoint().unwrap();
}

#[test]
fn full_ordinary_ledger_reserves_emergency_capacity_for_first_cancellations_only() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy_session) = rehearsal(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let active = running(&mut j, queued_first);
    let queued = fake_run(&mut j, &w, &legacy_session);
    let archived = j
        .audit_checkpoint()
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.state == v2::RunState::Interrupted)
        .unwrap();
    let count: u64 = j
        .connection()
        .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
        .unwrap();
    j.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
    for _ in count..COMMAND_RECEIPTS as u64 {
        let request = cancel_request(&archived);
        j.connection()
            .execute(
                "INSERT INTO receipts VALUES (?1,'cancel_run_v2',?2,?3,?4,0)",
                params![
                    w,
                    request.request_id,
                    canonical_arguments(&request).unwrap(),
                    encode(&archived).unwrap()
                ],
            )
            .unwrap();
    }
    j.connection().execute_batch("COMMIT").unwrap();
    // Ordinary ledger full: no-op cancels and appends' receipts are refused...
    assert_eq!(
        j.cancel_run(&cancel_request(&archived), None)
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    // ...but first state-changing cancellations still commit, as emergency.
    let request = cancel_request(&queued);
    j.cancel_run(&request, Some(to(&queued, v2::RunState::Cancelled)))
        .unwrap();
    let stopping = j
        .cancel_run(
            &cancel_request(&active),
            Some(to(&active, v2::RunState::CancelRequested)),
        )
        .unwrap()
        .result;
    // A later no-op on the same run cannot spend emergency capacity.
    assert_eq!(
        j.cancel_run(&cancel_request(&stopping), None)
            .err()
            .unwrap()
            .code,
        "capacity_exceeded"
    );
    assert!(j.cancel_run(&request, None).unwrap().replayed);
    let emergency: u64 = j
        .connection()
        .query_row("SELECT COUNT(*) FROM receipts WHERE emergency=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(emergency, 2);
    // Appends need no receipt, so the terminal stop path stays open.
    j.append_batch(to(&stopping, v2::RunState::Cancelled))
        .unwrap();
    j.audit_checkpoint().unwrap();
}

#[test]
fn layout_save_is_revisioned_receipted_and_preserves_unsupported_layouts() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    codex_run(&mut j, &w);
    let request_ws = v2::WorkspaceRequest {
        contract_version: 2,
        workspace_id: w.clone(),
    };
    let layout = j.workspace_snapshot(&request_ws).unwrap().layout.unwrap();
    let mut next = layout.clone();
    next.sidebar_width = if next.sidebar_width < 400.0 {
        next.sidebar_width + 16.0
    } else {
        260.0
    };
    let request = v2::SaveLayoutRequest {
        contract_version: 2,
        workspace_id: w.clone(),
        request_id: id(),
        expected_revision: layout.revision,
        layout: next.clone(),
    };
    let saved = j.save_layout(&request).unwrap();
    assert_eq!(saved.result.revision, layout.revision + 1);
    assert_eq!(
        j.workspace_snapshot(&request_ws).unwrap().layout.unwrap(),
        saved.result
    );
    let before = digest(j.connection());
    assert!(j.save_layout(&request).unwrap().replayed);
    let mut changed = request.clone();
    changed.layout.sidebar_width = 300.0;
    assert_eq!(
        j.save_layout(&changed).err().unwrap().code,
        "request_id_conflict"
    );
    let mut stale = request.clone();
    stale.request_id = id();
    assert_eq!(
        j.save_layout(&stale).err().unwrap().code,
        "layout_revision_conflict"
    );
    assert_eq!(digest(j.connection()), before);
    j.audit_checkpoint().unwrap();
    // Unsupported stored layout: preserved, never overwritten.
    j.connection()
        .execute(
            "UPDATE workspaces SET layout_json='{\"layout_version\":9}'",
            [],
        )
        .unwrap();
    let before = digest(j.connection());
    let mut fresh = request.clone();
    fresh.request_id = id();
    fresh.expected_revision = saved.result.revision;
    fresh.layout.revision = saved.result.revision;
    assert_eq!(
        j.save_layout(&fresh).err().unwrap().code,
        "unsupported_layout_version"
    );
    assert_eq!(digest(j.connection()), before);
}

#[test]
fn startup_recovery_interrupts_all_unfinished_v2_work_once_and_audits() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy_session) = rehearsal(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let active = running(&mut j, queued_first);
    let queued = fake_run(&mut j, &w, &legacy_session);
    let report = j.recover_for_test(LATER, &mut Ids).unwrap();
    // Every nonterminal run, queued included, in enqueue order: queued consent
    // and dispatch arms never survive a restart.
    let ids: Vec<_> = report
        .interrupted_runs
        .iter()
        .map(|r| r.run_id.clone())
        .collect();
    assert_eq!(ids, [active.run_id.clone(), queued.run_id.clone()]);
    assert!(report
        .interrupted_runs
        .iter()
        .all(|r| r.state == v2::RunState::Interrupted));
    assert_eq!(report.committed_events.len(), 2);
    assert!(report
        .committed_events
        .iter()
        .all(|e| matches!(e, v2::RunEvent::RunRecovered { .. })));
    assert!(j.next_queued(&w).unwrap().is_none());
    // Nothing unfinished: a repeat is an empty, write-free report.
    let before = digest(j.connection());
    assert!(j
        .recover_for_test(LATER, &mut Ids)
        .unwrap()
        .interrupted_runs
        .is_empty());
    assert_eq!(digest(j.connection()), before);
    assert!(j.recover_for_test("invalid", &mut Ids).is_err());
    // Work admitted after recovery is ordinary queued work...
    let after = fake_run(&mut j, &w, &legacy_session);
    assert_eq!(
        j.next_queued(&w).unwrap().unwrap().snapshot.run_id,
        after.run_id
    );
    // ...which the stateless function WOULD interrupt: proof that the C30 owner
    // must gate recovery to exactly once per open, before admission.
    assert_eq!(
        j.recover_for_test(LATER, &mut Ids)
            .unwrap()
            .interrupted_runs[0]
            .run_id,
        after.run_id
    );
    j.audit_checkpoint().unwrap();
}

#[test]
fn shared_writer_sql_does_not_hardcode_agent_profiles_models_or_dispatch() {
    let source = include_str!("../writer_v2.rs");
    for forbidden in [
        "CodexProfileId",
        "CodexModelId",
        "ProviderId::Codex",
        "codex_text_v1",
        "gpt-6.1-sol",
        "std::process",
        "spawn(",
    ] {
        assert!(
            !source.contains(forbidden),
            "adapter/execution policy leaked into shared persistence: {forbidden}"
        );
    }
}

#[test]
fn ordinary_event_limit_refuses_more_output_but_keeps_the_reserved_stop_path() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let queued_first = fake_run(&mut j, &w, &legacy);
    let mut run = running(&mut j, queued_first);
    let mut remaining = v2::ORDINARY_EVENTS - 3;
    while remaining > 0 {
        let count = remaining.min(v2::PAGE_EVENTS);
        let mut state = run.state;
        let flips = (0..count)
            .map(|_| {
                let next = if state == v2::RunState::Running {
                    v2::RunState::WaitingInput
                } else {
                    v2::RunState::Running
                };
                let payload = change(state, next);
                state = next;
                payload
            })
            .collect();
        run = j.append_batch(batch(&run, flips)).unwrap().result;
        remaining -= count;
    }
    assert_eq!(run.last_seq, v2::ORDINARY_EVENTS as u64);
    let before = digest(j.connection());
    let usage = v2::EventPayload::UsageUpdated(v2::Usage {
        input_tokens: None,
        output_tokens: None,
        estimated: false,
        cached_input_tokens: None,
        cache_write_input_tokens: None,
        reasoning_output_tokens: None,
    });
    assert_eq!(
        j.append_batch(batch(&run, vec![usage])).err().unwrap().code,
        "capacity_exceeded"
    );
    assert_eq!(digest(j.connection()), before);
    let marker = v2::EventPayload::OutputTruncated(v2::OutputTruncatedPayload {
        scope: v2::TruncationScope::EventCount,
        discarded_bytes: None,
        discarded_events: 1,
    });
    let stopping = j
        .append_batch(batch(
            &run,
            vec![marker, change(run.state, v2::RunState::CancelRequested)],
        ))
        .unwrap()
        .result;
    let done = j
        .append_batch(to(&stopping, v2::RunState::Cancelled))
        .unwrap()
        .result;
    assert_eq!(done.last_seq, v2::ORDINARY_EVENTS as u64 + 3);
    assert_eq!(
        counters(&mut j, &done.run_id),
        [v2::ORDINARY_EVENTS as u64, 3, 0, 4, 1, 1]
    );
    j.audit_checkpoint().unwrap();
}

#[test]
fn all_33_emergency_receipts_cancel_the_active_run_and_a_full_queue() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let queued_first = codex_run(&mut j, &w);
    let active = running(&mut j, queued_first);
    let mut outstanding = vec![active];
    outstanding.extend((0..QUEUED_RUNS).map(|_| fake_run(&mut j, &w, &legacy)));
    assert_eq!(outstanding.len(), CANCEL_RECEIPTS);
    let archived = j
        .audit_checkpoint()
        .unwrap()
        .runs
        .into_iter()
        .find(|r| r.state == v2::RunState::Interrupted)
        .unwrap();
    let count: u64 = j
        .connection()
        .query_row("SELECT COUNT(*) FROM receipts", [], |r| r.get(0))
        .unwrap();
    j.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
    for _ in count..COMMAND_RECEIPTS as u64 {
        let request = cancel_request(&archived);
        j.connection()
            .execute(
                "INSERT INTO receipts VALUES (?1,'cancel_run_v2',?2,?3,?4,0)",
                params![
                    w,
                    request.request_id,
                    canonical_arguments(&request).unwrap(),
                    encode(&archived).unwrap()
                ],
            )
            .unwrap();
    }
    j.connection().execute_batch("COMMIT").unwrap();
    for run in outstanding {
        let target = if run.state == v2::RunState::Queued {
            v2::RunState::Cancelled
        } else {
            v2::RunState::CancelRequested
        };
        let request = cancel_request(&run);
        let saved = j
            .cancel_run(&request, Some(to(&run, target)))
            .unwrap()
            .result;
        assert_eq!(saved.state, target);
        assert!(j.cancel_run(&request, None).unwrap().replayed);
    }
    let emergency: u64 = j
        .connection()
        .query_row("SELECT COUNT(*) FROM receipts WHERE emergency=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(emergency, CANCEL_RECEIPTS as u64);
    j.audit_checkpoint().unwrap();
}
