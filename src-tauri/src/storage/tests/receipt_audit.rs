use super::*;

struct Fixture {
    create: CreateSessionRequest,
    start: StartFakeRunRequest,
    cancel: CancelRunRequest,
    layout: SaveLayoutRequest,
}
fn fixture(directory: &Path) -> (SqliteStore, Fixture) {
    let (mut store, workspace_id, _) = fresh(directory);
    let create = create_request(&workspace_id);
    let session = store
        .create_session(&create, session_seed())
        .unwrap()
        .result;
    let start = start_request(&workspace_id, &session.session_id);
    let queued = store.enqueue_run(&start, run_seed()).unwrap().result;
    let active = running(&mut store, queued);
    let cancel = cancel_request(&active);
    let requested = store
        .cancel_run(
            &cancel,
            Some(transition(&active, RunState::CancelRequested)),
        )
        .unwrap()
        .result;
    let terminal = store
        .append_batch(transition(&requested, RunState::Cancelled))
        .unwrap()
        .result;
    store.cancel_run(&cancel_request(&terminal), None).unwrap();
    let layout = SaveLayoutRequest {
        contract_version: 1,
        workspace_id: workspace_id.clone(),
        request_id: id(),
        expected_revision: 0,
        layout: empty_layout(&workspace_id),
    };
    let saved = store.save_layout(&layout).unwrap().result;
    let mut later = SaveLayoutRequest {
        request_id: id(),
        expected_revision: saved.revision,
        layout: saved,
        ..layout.clone()
    };
    later.layout.theme = ThemeName::Light;
    store.save_layout(&later).unwrap();
    (
        store,
        Fixture {
            create,
            start,
            cancel,
            layout,
        },
    )
}

#[test]
fn startup_audit_rejects_damaged_receipts_before_original_writes() {
    let mut accepted = Vec::new();
    for damage in [
        "request_uuid",
        "request_key",
        "arguments_json",
        "arguments_unknown",
        "arguments_version",
        "arguments_workspace",
        "arguments_noncanonical",
        "arguments_label",
        "result_json",
        "result_unknown",
        "result_session_id",
        "result_label",
        "start_missing_null",
        "start_run_id",
        "start_state",
        "start_time",
        "start_prompt",
        "start_scenario",
        "cancel_target",
        "cancel_time",
        "cancel_usage",
        "cancel_future_seq",
        "cancel_emergency_completion",
        "layout_revision",
        "layout_theme",
        "layout_expected_revision",
        "layout_unknown",
        "oversized_arguments",
        "oversized_result",
    ] {
        let directory = TemporaryDirectory::new();
        let (mut store, fixture) = fixture(&directory.0);
        let (command, request_id) = if damage.starts_with("start_") {
            ("start_fake_run", &fixture.start.request_id)
        } else if damage.starts_with("cancel_") {
            ("cancel_run", &fixture.cancel.request_id)
        } else if damage.starts_with("layout_") {
            ("save_layout", &fixture.layout.request_id)
        } else {
            ("create_session", &fixture.create.request_id)
        };
        let (arguments, result): (String, String) = store.conn.query_row("SELECT arguments_json,result_json FROM receipts WHERE command_name=?1 AND request_id=?2", params![command,request_id], |r| Ok((r.get(0)?,r.get(1)?))).unwrap();
        let mut args: serde_json::Value = serde_json::from_str(&arguments).unwrap();
        let mut reply: serde_json::Value = serde_json::from_str(&result).unwrap();
        match damage {
            "arguments_unknown" => args["unexpected"] = true.into(),
            "arguments_version" => args["contract_version"] = 2.into(),
            "arguments_workspace" => args["workspace_id"] = id().into(),
            "arguments_label" => args["label"] = "Different but valid".into(),
            "result_unknown" => reply["unexpected"] = true.into(),
            "result_session_id" => reply["session_id"] = id().into(),
            "result_label" => reply["label"] = "Different but valid".into(),
            "start_missing_null" => {
                reply["usage"]
                    .as_object_mut()
                    .unwrap()
                    .remove("input_tokens");
            }
            "start_run_id" => reply["run_id"] = id().into(),
            "start_state" => {
                reply["state"] = "running".into();
                reply["last_seq"] = 3.into();
                reply["updated_at"] = LATER.into();
            }
            "start_time" | "cancel_time" => reply["created_at"] = LATER.into(),
            "start_prompt" => args["prompt"] = "Different but valid".into(),
            "start_scenario" => args["scenario"] = "failure".into(),
            "cancel_target" => args["run_id"] = id().into(),
            "cancel_usage" => reply["usage"]["estimated"] = true.into(),
            "cancel_future_seq" => reply["last_seq"] = 100.into(),
            "cancel_emergency_completion" => {
                let terminal = store
                    .get_run(&fixture.cancel.workspace_id, &fixture.cancel.run_id)
                    .unwrap()
                    .snapshot;
                reply = serde_json::to_value(terminal).unwrap();
                store
                    .conn
                    .execute(
                        "UPDATE receipts SET emergency=1 WHERE command_name=?1 AND request_id=?2",
                        params![command, request_id],
                    )
                    .unwrap();
            }
            "layout_revision" => reply["revision"] = 2.into(),
            "layout_theme" => reply["theme"] = "light".into(),
            "layout_expected_revision" => {
                args["expected_revision"] = 1.into();
                args["layout"]["revision"] = 1.into();
            }
            "layout_unknown" => args["layout"]["unexpected"] = true.into(),
            _ => {}
        }
        let mut arguments = canonical_arguments(&args).unwrap();
        let mut result = serde_json::to_string(&reply).unwrap();
        match damage {
            "request_uuid" => {
                store.conn.execute("UPDATE receipts SET request_id='not-a-uuid' WHERE command_name=?1 AND request_id=?2",params![command,request_id]).unwrap();
            }
            "request_key" => {
                store
                    .conn
                    .execute(
                        "UPDATE receipts SET request_id=?1 WHERE command_name=?2 AND request_id=?3",
                        params![id(), command, request_id],
                    )
                    .unwrap();
            }
            "arguments_json" => arguments = "{".into(),
            "result_json" => result = "{".into(),
            "arguments_noncanonical" => arguments.push(' '),
            "oversized_arguments" => arguments.push_str(&" ".repeat(1_048_577)),
            "oversized_result" => result.push_str(&" ".repeat(1_048_577)),
            _ => {}
        }
        if !matches!(damage, "request_uuid" | "request_key") {
            store.conn.execute("UPDATE receipts SET arguments_json=?1,result_json=?2 WHERE command_name=?3 AND request_id=?4",params![arguments,result,command,request_id]).unwrap();
        }
        drop(store);
        let database = directory.0.join("execution.sqlite3");
        let before = fs::read(&database).unwrap();
        match SqliteStore::open(&directory.0) {
            Ok(store) => {
                accepted.push(damage);
                drop(store);
            }
            Err(failure) => {
                assert_eq!(failure.code, "storage_unavailable", "{damage}");
                assert_eq!(fs::read(&database).unwrap(), before, "{damage}");
                assert!(!directory.0.join("execution.sqlite3-journal").exists());
            }
        }
    }
    assert!(
        accepted.is_empty(),
        "startup incorrectly accepted damaged receipt cases: {accepted:?}"
    );
}

#[test]
fn historical_receipts_remain_exact_after_reopen_and_later_mutations() {
    let directory = TemporaryDirectory::new();
    let (mut store, fixture) = fixture(&directory.0);
    let session = store
        .create_session(&fixture.create, session_seed())
        .unwrap()
        .result;
    let initial = store
        .enqueue_run(&fixture.start, run_seed())
        .unwrap()
        .result;
    let cancellation = store.cancel_run(&fixture.cancel, None).unwrap().result;
    let old_layout = store.save_layout(&fixture.layout).unwrap().result;
    let before = stored_count(&store, "receipts");
    drop(store);
    let mut store = SqliteStore::open(&directory.0).unwrap();
    assert!(store
        .startup_recover(LATER, &mut Ids)
        .unwrap()
        .committed_events
        .is_empty());
    let created = store
        .create_session(&fixture.create, session_seed())
        .unwrap();
    let started = store.enqueue_run(&fixture.start, run_seed()).unwrap();
    let cancelled = store.cancel_run(&fixture.cancel, None).unwrap();
    let saved = store.save_layout(&fixture.layout).unwrap();
    assert!(created.replayed && started.replayed && cancelled.replayed && saved.replayed);
    assert_eq!(created.result, session);
    assert_eq!(started.result, initial);
    assert_eq!(cancelled.result, cancellation);
    assert_eq!(saved.result, old_layout);
    assert_eq!(initial.state, RunState::Queued);
    assert_eq!(cancellation.state, RunState::CancelRequested);
    assert_eq!(
        store
            .get_run(&fixture.cancel.workspace_id, &fixture.cancel.run_id)
            .unwrap()
            .snapshot
            .state,
        RunState::Cancelled
    );
    assert_eq!(stored_count(&store, "receipts"), before);
}

#[test]
fn damaged_receipt_in_hot_journal_preserves_original_family() {
    let directory = TemporaryDirectory::new();
    let (store, fixture) = fixture(&directory.0);
    store
        .conn
        .execute(
            "UPDATE receipts SET result_json='{' WHERE request_id=?1",
            [&fixture.cancel.request_id],
        )
        .unwrap();
    drop(store);
    leave_hot_journal(&directory);
    let database = directory.0.join("execution.sqlite3");
    let journal = directory.0.join("execution.sqlite3-journal");
    let before = (fs::read(&database).unwrap(), fs::read(&journal).unwrap());
    assert_eq!(
        SqliteStore::open(&directory.0)
            .err()
            .expect("invalid recovered receipt")
            .code,
        "storage_unavailable"
    );
    assert_eq!(fs::read(&database).unwrap(), before.0);
    assert_eq!(fs::read(&journal).unwrap(), before.1);
    assert!(validation_children(&directory.0).is_empty());
}

#[test]
fn valid_receipts_allow_current_layout_warning_without_replacement() {
    for newer in [false, true] {
        let directory = TemporaryDirectory::new();
        let (store, _) = fixture(&directory.0);
        let raw = if newer {
            r#"{"layout_version":2}"#
        } else {
            "{invalid"
        };
        store
            .conn
            .execute("UPDATE workspaces SET layout_json=?1", [raw])
            .unwrap();
        drop(store);
        let mut reopened = SqliteStore::open(&directory.0).unwrap();
        let snapshot = reopened.bootstrap(workspace_seed()).unwrap();
        assert!(snapshot.layout.is_none());
        assert_eq!(
            snapshot.layout_warning.unwrap().code,
            if newer {
                "unsupported_layout_version"
            } else {
                "layout_invalid"
            }
        );
        assert!(reopened
            .startup_recover(LATER, &mut Ids)
            .unwrap()
            .committed_events
            .is_empty());
        let unchanged: String = reopened
            .conn
            .query_row("SELECT layout_json FROM workspaces", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unchanged, raw);
    }
}

#[test]
fn maximum_escaped_prompt_and_unicode_label_receipts_reopen() {
    let directory = TemporaryDirectory::new();
    let (mut store, workspace_id, _) = fresh(&directory.0);
    let mut create = create_request(&workspace_id);
    create.label = "🧪".repeat(128);
    let session = store
        .create_session(&create, session_seed())
        .unwrap()
        .result;
    let mut request = start_request(&workspace_id, &session.session_id);
    request.prompt = "	".repeat(PROMPT_BYTES);
    let original = store.enqueue_run(&request, run_seed()).unwrap().result;
    assert!(canonical_arguments(&request).unwrap().len() > PROMPT_BYTES * 2);
    drop(store);
    let mut reopened = SqliteStore::open(&directory.0).unwrap();
    assert_eq!(
        reopened
            .startup_recover(LATER, &mut Ids)
            .unwrap()
            .interrupted_runs
            .len(),
        1
    );
    assert_eq!(
        reopened.enqueue_run(&request, run_seed()).unwrap().result,
        original
    );
    assert_eq!(
        reopened
            .create_session(&create, session_seed())
            .unwrap()
            .result,
        session
    );
}
