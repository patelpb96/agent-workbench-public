//! Direct-read tests use native COPY transactions/SQL fixtures, never dispatch.
use super::admission_v2::{codex, create, digest, fake, family, rehearsal, terminal_fixture};
use super::*;
use crate::execution_v2::{
    self as v2,
    admission::StartRequest,
    ports::{AdmissionJournal, JournalReader, QueuedInput},
    validation as policy,
};
use crate::storage::admission_v2::AdmissionRehearsal;

fn request(workspace: &str) -> v2::WorkspaceRequest {
    v2::WorkspaceRequest {
        contract_version: 2,
        workspace_id: workspace.into(),
    }
}
fn page(
    run: &v2::RunSnapshot,
    after: u64,
    through: Option<u64>,
    limit: u64,
) -> v2::EventPageRequest {
    v2::EventPageRequest {
        contract_version: 2,
        workspace_id: run.workspace_id.clone(),
        run_id: run.run_id.clone(),
        after_seq: after,
        through_seq: through,
        limit,
    }
}
fn append_fixture(
    j: &mut AdmissionRehearsal,
    current: &v2::RunSnapshot,
    payload: v2::EventPayload,
) -> v2::RunSnapshot {
    let source = match &payload {
        v2::EventPayload::TextDelta(_) | v2::EventPayload::UsageUpdated(_) => {
            if current.provider_id == v2::ProviderId::Codex {
                v2::EventSource::Provider
            } else {
                v2::EventSource::Fixture
            }
        }
        _ => v2::EventSource::System,
    };
    let event = v2::RunEvent::from_parts(
        v2::EventHeader {
            contract_version: 2,
            event_id: id(),
            workspace_id: current.workspace_id.clone(),
            session_id: current.session_id.clone(),
            run_id: current.run_id.clone(),
            provider_id: current.provider_id,
            source,
            seq: current.last_seq + 1,
            occurred_at: LATER.into(),
        },
        payload,
    );
    let next = policy::apply_event(current, &event).unwrap();
    let mut counts:[u64;6]=j.connection().query_row("SELECT ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count FROM runs WHERE run_id=?1",[&next.run_id],
        |r|Ok([r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?])).unwrap();
    match &event {
        v2::RunEvent::RunRecovered { .. } => {
            counts[1] += 1;
            counts[5] += 1;
        }
        v2::RunEvent::RunStateChanged { payload, .. }
            if payload.to == v2::RunState::CancelRequested =>
        {
            counts[1] += 1;
            counts[4] += 1;
        }
        v2::RunEvent::RunStateChanged { payload, .. }
            if matches!(
                payload.to,
                v2::RunState::Succeeded
                    | v2::RunState::Failed
                    | v2::RunState::Cancelled
                    | v2::RunState::Interrupted
            ) =>
        {
            counts[1] += 1;
            counts[5] += 1;
        }
        _ => {
            counts[0] += 1;
            if let v2::RunEvent::TextDelta { payload, .. } = &event {
                counts[2] += payload.text.len() as u64;
            }
        }
    }
    let h = event.header();
    j.connection()
        .execute(
            "INSERT INTO events VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                h.event_id,
                h.workspace_id,
                h.session_id,
                h.run_id,
                h.seq,
                encode(&event).unwrap()
            ],
        )
        .unwrap();
    j.connection().execute("UPDATE runs SET snapshot_json=?1,state=?2,last_seq=?3,ordinary_count=?4,reserved_count=?5,text_bytes=?6,truncation_mask=?7,cancel_count=?8,terminal_count=?9 WHERE run_id=?10",
        params![encode(&next).unwrap(),next.state.as_str(),next.last_seq,counts[0],counts[1],counts[2],counts[3],counts[4],counts[5],next.run_id]).unwrap();
    next
}
fn state(
    j: &mut AdmissionRehearsal,
    current: &v2::RunSnapshot,
    to: v2::RunState,
) -> v2::RunSnapshot {
    append_fixture(
        j,
        current,
        v2::EventPayload::RunStateChanged(v2::RunStateChangedPayload {
            from: current.state,
            to,
            error: None,
        }),
    )
}
fn active(j: &mut AdmissionRehearsal, w: &str) -> v2::RunSnapshot {
    let session = j
        .create_session(&create(w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    let run = j
        .enqueue_run(
            StartRequest::CodexText(&codex(w, &session.session_id)),
            run_seed(),
        )
        .unwrap()
        .result;
    let run = state(j, &run, v2::RunState::Starting);
    state(j, &run, v2::RunState::Running)
}
fn texts(j: &mut AdmissionRehearsal, mut run: v2::RunSnapshot, count: usize) -> v2::RunSnapshot {
    j.connection().execute_batch("BEGIN IMMEDIATE").unwrap();
    for index in 0..count {
        run = append_fixture(
            j,
            &run,
            v2::EventPayload::TextDelta(v2::TextDeltaPayload {
                stream: v2::TextStream::Assistant,
                text: format!("Escaped <fixture> 😀 {index}\n"),
            }),
        );
    }
    j.connection().execute_batch("COMMIT").unwrap();
    run
}

#[test]
fn direct_snapshot_matches_full_copy_audit_without_exposing_private_input() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let original = family(&directory.0);
    let session = j
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    let mut input = codex(&w, &session.session_id);
    input.prompt = "PRIVATE-READ-ONLY-PROMPT-😀".into();
    let run = j
        .enqueue_run(StartRequest::CodexText(&input), run_seed())
        .unwrap()
        .result;
    j.enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
        .unwrap();
    let before = digest(j.connection());
    let snapshot = j.workspace_snapshot(&request(&w)).unwrap();
    assert_eq!(snapshot, j.audit_checkpoint().unwrap());
    assert!(!snapshot.execution_available);
    assert!(!serde_json::to_string(&snapshot)
        .unwrap()
        .contains(&input.prompt));
    let private = j.get_run(&w, &run.run_id).unwrap();
    assert_eq!(private.snapshot, run);
    assert!(matches!(private.input,QueuedInput::CodexText { prompt,.. } if prompt==input.prompt));
    let archived = snapshot
        .runs
        .iter()
        .find(|r| r.state == v2::RunState::Interrupted)
        .unwrap();
    assert!(matches!(
        j.get_run(&w, &archived.run_id).unwrap().input,
        QueuedInput::Fake { .. }
    ));
    assert_eq!(digest(j.connection()), before);
    assert_eq!(family(&directory.0), original);
    j.close().unwrap();
    assert_eq!(family(&directory.0), original);
}

#[test]
fn legacy_history_projects_at_read_but_never_rewrites_original_json_or_receipts() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let legacy = j.workspace_snapshot(&request(&w)).unwrap().runs.remove(0);
    let before = digest(j.connection());
    let value = j.list_run_events(&page(&legacy, 0, None, 200)).unwrap();
    assert_eq!(value.events.len(), 2);
    assert!(!value.has_more);
    assert_eq!(value.next_after_seq, 2);
    assert!(value
        .events
        .iter()
        .all(|e| e.header().contract_version == 2));
    assert!(matches!(value.events[1], v2::RunEvent::RunRecovered { .. }));
    assert_eq!(digest(j.connection()), before);
    let versions: Vec<u64> = j
        .connection()
        .prepare("SELECT json_extract(event_json,'$.contract_version') FROM events")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(versions.iter().all(|v| *v == 1));
}

#[test]
fn native_codex_pages_are_contiguous_bounded_and_pin_their_high_water() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let run = active(&mut j, &w);
    let run = texts(&mut j, run, 451);
    let pinned = run.last_seq;
    let first = j.list_run_events(&page(&run, 0, None, 17)).unwrap();
    assert_eq!(first.events.len(), 17);
    assert!(first.has_more);
    let run = texts(&mut j, run, 9);
    let mut collected = first.events;
    let mut cursor = 17;
    while cursor < pinned {
        let value = j
            .list_run_events(&page(&run, cursor, Some(pinned), 200))
            .unwrap();
        assert!(value.events.len() <= 200);
        assert_eq!(value.last_seq, run.last_seq);
        assert_eq!(value.through_seq, pinned);
        cursor = value.next_after_seq;
        assert_eq!(value.has_more, cursor < pinned);
        collected.extend(value.events);
    }
    assert_eq!(collected.len() as u64, pinned);
    for (index, event) in collected.iter().enumerate() {
        assert_eq!(event.header().seq, index as u64 + 1);
        assert_eq!(event.header().provider_id, v2::ProviderId::Codex);
    }
    let tail = j.list_run_events(&page(&run, pinned, None, 200)).unwrap();
    assert_eq!(tail.events.len(), 9);
    assert!(!tail.has_more);
    assert_eq!(tail.next_after_seq, run.last_seq);
    let empty = j
        .list_run_events(&page(&run, run.last_seq, None, 1))
        .unwrap();
    assert!(empty.events.is_empty() && !empty.has_more);
    let zero = j.list_run_events(&page(&run, 0, Some(0), 1)).unwrap();
    assert!(zero.events.is_empty() && !zero.has_more);
    assert_eq!(
        j.workspace_snapshot(&request(&w)).unwrap(),
        j.audit_checkpoint().unwrap()
    );
}

#[test]
fn fifo_private_reads_do_not_consume_queue_or_confer_a_dispatch_arm() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    assert!(j.next_queued(&w).unwrap().is_none());
    let fake_run = j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
        .unwrap()
        .result;
    let session = j
        .create_session(&create(&w, v2::ProviderId::Codex), session_seed())
        .unwrap()
        .result;
    let codex_run = j
        .enqueue_run(
            StartRequest::CodexText(&codex(&w, &session.session_id)),
            run_seed(),
        )
        .unwrap()
        .result;
    let before = digest(j.connection());
    for _ in 0..3 {
        assert_eq!(j.next_queued(&w).unwrap().unwrap().snapshot, fake_run);
    }
    assert_eq!(digest(j.connection()), before);
    let starting = state(&mut j, &fake_run, v2::RunState::Starting);
    assert!(j.next_queued(&w).unwrap().is_none());
    let running = state(&mut j, &starting, v2::RunState::Running);
    assert!(j.next_queued(&w).unwrap().is_none());
    let waiting = state(&mut j, &running, v2::RunState::WaitingInput);
    assert!(j.next_queued(&w).unwrap().is_none());
    let cancel = state(&mut j, &waiting, v2::RunState::CancelRequested);
    assert!(j.next_queued(&w).unwrap().is_none());
    state(&mut j, &cancel, v2::RunState::Cancelled);
    let next = j.next_queued(&w).unwrap().unwrap();
    assert_eq!(next.snapshot, codex_run);
    assert!(matches!(next.input, QueuedInput::CodexText { .. }));
    assert_eq!(
        j.workspace_snapshot(&request(&w)).unwrap(),
        j.audit_checkpoint().unwrap()
    );
}

#[test]
fn query_only_reads_do_not_scan_receipts_or_unrequested_event_ranges() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    let run = active(&mut j, &w);
    let run = texts(&mut j, run, 30);
    // Intentional post-constructor damage proves normal reads are NOT a full
    // integrity certificate. The requested prefix remains independently valid.
    j.connection()
        .execute(
            "UPDATE receipts SET result_json='{}' WHERE command_name='start_codex_run_v2'",
            [],
        )
        .unwrap();
    j.connection()
        .execute(
            "DELETE FROM events WHERE run_id=?1 AND seq=20",
            [&run.run_id],
        )
        .unwrap();
    j.connection()
        .pragma_update(None, "query_only", true)
        .unwrap();
    let before = digest(j.connection());
    let original = family(&directory.0);
    j.workspace_snapshot(&request(&w)).unwrap();
    j.get_run(&w, &run.run_id).unwrap();
    assert!(j.next_queued(&w).unwrap().is_none());
    let value = j.list_run_events(&page(&run, 0, Some(3), 3)).unwrap();
    assert_eq!(value.events.len(), 3);
    assert!(j.list_run_events(&page(&run, 19, Some(21), 2)).is_err());
    assert!(j.audit_checkpoint().is_err());
    assert_eq!(digest(j.connection()), before);
    assert_eq!(family(&directory.0), original);
}

#[test]
fn invalid_scope_versions_limits_and_cursors_fail_without_writes() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    let run = j
        .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
        .unwrap()
        .result;
    let before = digest(j.connection());
    assert!(j
        .workspace_snapshot(&v2::WorkspaceRequest {
            contract_version: 1,
            workspace_id: w.clone()
        })
        .is_err());
    assert_eq!(
        j.workspace_snapshot(&request(&id())).err().unwrap().code,
        "workspace_not_found"
    );
    assert_eq!(j.get_run(&w, &id()).err().unwrap().code, "run_not_found");
    assert!(j.get_run("invalid", &run.run_id).is_err());
    assert!(j.next_queued("invalid").is_err());
    assert!(j.next_queued(&id()).is_err());
    let mut cases = vec![
        page(&run, 2, None, 1),
        page(&run, 0, Some(2), 1),
        page(&run, 1, Some(0), 1),
        page(&run, 0, None, 0),
        page(&run, 0, None, 201),
    ];
    let mut r = page(&run, 0, None, 1);
    r.contract_version = 1;
    cases.push(r);
    let mut r = page(&run, 0, None, 1);
    r.run_id = id();
    cases.push(r);
    let mut r = page(&run, 0, None, 1);
    r.workspace_id = id();
    cases.push(r);
    for r in cases {
        assert!(j.list_run_events(&r).is_err());
    }
    assert_eq!(digest(j.connection()), before);
}

#[test]
fn mixed_original_event_versions_and_page_corruption_are_rejected() {
    let changes=[
        "UPDATE events SET event_json=json_set(event_json,'$.contract_version',1) WHERE json_extract(event_json,'$.contract_version')=2",
        "UPDATE events SET event_json=json_set(event_json,'$.contract_version',2) WHERE json_extract(event_json,'$.contract_version')=1",
        "UPDATE events SET event_json=json_set(event_json,'$.event_id','00000000-0000-4000-8000-000000000000') WHERE json_extract(event_json,'$.contract_version')=2",
        "UPDATE events SET event_json=json_set(event_json,'$.seq',2) WHERE json_extract(event_json,'$.contract_version')=2",
        "UPDATE events SET event_json=json_set(event_json,'$.session_id','00000000-0000-4000-8000-000000000000') WHERE json_extract(event_json,'$.contract_version')=2",
        "UPDATE events SET event_json=json_set(event_json,'$.extra',true) WHERE json_extract(event_json,'$.contract_version')=2",
        "UPDATE events SET event_json=event_json||printf('%*s',20481,' ') WHERE json_extract(event_json,'$.contract_version')=2",
        "UPDATE events SET event_json=CAST(x'80' AS TEXT) WHERE json_extract(event_json,'$.contract_version')=2",
        "DELETE FROM events WHERE json_extract(event_json,'$.contract_version')=2",
    ];
    for sql in changes {
        let directory = TemporaryDirectory::new();
        let (mut j, w, legacy) = rehearsal(&directory.0);
        let run = j
            .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
            .unwrap()
            .result;
        let archived = j.workspace_snapshot(&request(&w)).unwrap().runs[0].clone();
        j.connection().execute_batch(sql).unwrap();
        // Invalid UTF-8 TEXT must be observed as bytes, not through the shared
        // JSON/String digest helper whose decoding would fail before the read.
        let before = j.connection().total_changes();
        let target = if sql.contains("=1")
            && sql.starts_with(
                "UPDATE events SET event_json=json_set(event_json,'$.contract_version',2)",
            ) {
            &archived
        } else {
            &run
        };
        assert!(
            j.list_run_events(&page(target, 0, None, 200)).is_err(),
            "accepted {sql}"
        );
        assert_eq!(j.connection().total_changes(), before);
    }
}

#[test]
fn corrupt_materialized_rows_private_inputs_and_counters_fail_closed() {
    let changes=[
        "UPDATE runs SET snapshot_json=json_set(snapshot_json,'$.last_seq',4) WHERE provider_id='codex'",
        "UPDATE runs SET text_bytes=10485761 WHERE provider_id='codex'",
        "UPDATE runs SET reserved_count=1,ordinary_count=0 WHERE provider_id='codex'",
        "UPDATE runs SET truncation_mask=1 WHERE provider_id='codex'",
        "UPDATE runs SET provider_id='fake' WHERE provider_id='codex'",
        "UPDATE runs SET profile_id=NULL WHERE provider_id='codex'",
        "UPDATE runs SET scenario='success' WHERE provider_id='codex'",
        "UPDATE runs SET prompt=printf('%*s',1025,'x') WHERE provider_id='codex'",
        "UPDATE runs SET profile_id=printf('%*s',50000,'x') WHERE provider_id='codex'",
        "UPDATE runs SET scenario=printf('%*s',50000,'x') WHERE provider_id='codex'",
        "UPDATE runs SET prompt=CAST(x'80' AS TEXT) WHERE provider_id='codex'",
        "UPDATE sessions SET snapshot_json=json_set(snapshot_json,'$.provider_id','fake') WHERE json_extract(snapshot_json,'$.provider_id')='codex'",
        "UPDATE workspaces SET record_json=printf('%*s',16385,'x')",
        "UPDATE workspaces SET layout_json=printf('%*s',1048577,'x')",
    ];
    for sql in changes {
        let directory = TemporaryDirectory::new();
        let (mut j, w, _) = rehearsal(&directory.0);
        let run = active(&mut j, &w);
        j.connection()
            .execute_batch("PRAGMA ignore_check_constraints=ON")
            .unwrap();
        j.connection().execute_batch(sql).unwrap();
        assert!(
            j.workspace_snapshot(&request(&w)).is_err(),
            "accepted {sql}"
        );
        assert!(j.get_run(&w, &run.run_id).is_err(), "accepted input {sql}");
        assert!(j.next_queued(&w).is_err(), "accepted queue {sql}");
    }
}

#[test]
fn normal_reads_preserve_layout_warning_semantics_and_recovered_legacy_gate() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, _) = rehearsal(&directory.0);
    for raw in ["{", "{\"layout_version\":9}"] {
        j.connection()
            .execute("UPDATE workspaces SET layout_json=?1", [raw])
            .unwrap();
        let snapshot = j.workspace_snapshot(&request(&w)).unwrap();
        assert_eq!(snapshot, j.audit_checkpoint().unwrap());
        assert!(snapshot.layout.is_none() && snapshot.layout_warning.is_some());
    }
    // A forged unfinished legacy materialization must not be offered by the
    // post-recovery reader, even though a full startup auditor also rejects it.
    j.connection().execute("UPDATE runs SET snapshot_json=json_set(snapshot_json,'$.state','queued','$.last_seq',1),state='queued',last_seq=1,reserved_count=0,terminal_count=0",[]).unwrap();
    assert!(j.next_queued(&w).is_err());
    assert!(j.workspace_snapshot(&request(&w)).is_err());
}

#[test]
fn direct_read_row_caps_accept_full_workspace_then_reject_overflow() {
    let directory = TemporaryDirectory::new();
    let (mut j, w, legacy) = rehearsal(&directory.0);
    for _ in 1..WORKSPACE_SESSIONS {
        j.create_session(&create(&w, v2::ProviderId::Fake), session_seed())
            .unwrap();
    }
    for _ in 1..WORKSPACE_RUNS {
        let run = j
            .enqueue_run(StartRequest::Fake(&fake(&w, &legacy)), run_seed())
            .unwrap()
            .result;
        terminal_fixture(&mut j, &run);
    }
    assert_eq!(
        j.workspace_snapshot(&request(&w)).unwrap(),
        j.audit_checkpoint().unwrap()
    );
    let raw: String = j
        .connection()
        .query_row("SELECT snapshot_json FROM sessions LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    let mut value: serde_json::Value = decode(&raw).unwrap();
    value["session_id"] = id().into();
    j.connection()
        .execute(
            "INSERT INTO sessions VALUES (?1,?2,?3)",
            params![value["session_id"].as_str(), w, encode(&value).unwrap()],
        )
        .unwrap();
    assert!(j.workspace_snapshot(&request(&w)).is_err());
    j.connection()
        .execute(
            "DELETE FROM sessions WHERE session_id=?1",
            [value["session_id"].as_str().unwrap()],
        )
        .unwrap();
    let raw: String = j
        .connection()
        .query_row("SELECT snapshot_json FROM runs LIMIT 1", [], |r| r.get(0))
        .unwrap();
    let mut value: serde_json::Value = decode(&raw).unwrap();
    value["run_id"] = id().into();
    value["enqueue_order"] = 201.into();
    j.connection()
        .execute("UPDATE workspaces SET last_enqueue=201", [])
        .unwrap();
    j.connection().execute("INSERT INTO runs SELECT ?1,workspace_id,session_id,state,last_seq,201,?2,prompt,scenario,ordinary_count,reserved_count,text_bytes,truncation_mask,cancel_count,terminal_count,provider_id,profile_id,context_json FROM runs LIMIT 1",
        params![value["run_id"].as_str(),encode(&value).unwrap()]).unwrap();
    assert!(j.workspace_snapshot(&request(&w)).is_err());
}
