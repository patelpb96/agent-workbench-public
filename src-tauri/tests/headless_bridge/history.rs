//! Fixed native v2 fixture for browser paging tests. No raw SQL or alternate runtime:
//! every write goes through the fresh V2Store's audited journal ports.
use super::{validate_lease, Lease};
use agent_workbench::{
    execution::ports::{IdSource, RunSeed, SessionSeed, StoreResult, WorkspaceSeed},
    execution_v2::{
        admission::StartRequest,
        ports::{AdmissionJournal, AppendBatch, JournalWriter},
        validation::apply_event,
        *,
    },
    storage::store_v2::V2Store,
};
use std::{fs, io};
const NOW: &str = "2026-10-20T17:00:00.000Z";
const TEXT_EVENTS: usize = 600;
const TEXT_BYTES: usize = 8192;
fn id() -> Id {
    uuid::Uuid::new_v4().to_string()
}
struct Ids;
impl IdSource for Ids {
    fn next_id(&mut self) -> Id {
        id()
    }
}
fn checked<T>(result: StoreResult<T>) -> io::Result<T> {
    result.map_err(|_| io::Error::other("native history fixture validation/commit failed"))
}
fn append(store: &mut V2Store, current: &RunSnapshot, payloads: Vec<EventPayload>) -> io::Result<RunSnapshot> {
    let mut next = current.clone();
    let mut events = Vec::new();
    for payload in payloads {
        let source = if matches!(&payload, EventPayload::TextDelta(_)) { EventSource::Fixture } else { EventSource::System };
        let event = RunEvent::from_parts(EventHeader { contract_version: 2, event_id: id(),
            workspace_id: current.workspace_id.clone(), session_id: current.session_id.clone(),
            run_id: current.run_id.clone(), provider_id: ProviderId::Fake, source,
            seq: next.last_seq + 1, occurred_at: NOW.into() }, payload);
        next = checked(apply_event(&next, &event))?;
        events.push(event);
    }
    Ok(checked(store.append_batch(AppendBatch { workspace_id: current.workspace_id.clone(),
        run_id: current.run_id.clone(), expected_last_seq: current.last_seq,
        expected_state: current.state, events, next_snapshot: next }))?.result)
}
fn state(from: RunState, to: RunState) -> EventPayload {
    EventPayload::RunStateChanged(RunStateChangedPayload { from, to, error: None })
}
pub(super) fn seed(lease: &Lease) -> io::Result<()> {
    let workspace = validate_lease(lease)?.join("workspace");
    if fs::read_dir(&workspace)?.next().is_some() { return Err(io::Error::other("history fixture requires an empty owned workspace")); }
    let mut store = checked(V2Store::open(&workspace))?;
    let initial = checked(store.bootstrap(WorkspaceSeed { workspace_id: id(), created_at: NOW.into() }))?;
    checked(store.startup_recover(NOW, &mut Ids))?;
    let workspace_id = initial.workspace.workspace_id;
    let session = checked(store.create_session(&CreateSessionRequest { contract_version: 2,
        workspace_id: workspace_id.clone(), request_id: id(), label: "SIMULATED paged history".into(),
        provider_id: ProviderId::Fake },
        SessionSeed { session_id: id(), created_at: NOW.into() }))?.result;
    let start = StartFakeRunRequest { contract_version: 2,
        workspace_id: workspace_id.clone(), session_id: session.session_id.clone(), request_id: id(),
        provider_id: ProviderId::Fake, prompt: "SIMULATED native history fixture, never execute".into(), scenario: FakeScenario::Success };
    let mut run = checked(store.enqueue_run(StartRequest::Fake(&start),
        RunSeed { run_id: id(), event_id: id(), created_at: NOW.into() }))?.result;
    run = append(&mut store, &run, vec![state(RunState::Queued, RunState::Starting), state(RunState::Starting, RunState::Running)])?;
    // 64 * (8 KiB + envelope) stays below the real 1 MiB pending batch limit.
    for start in (0..TEXT_EVENTS).step_by(64) {
        let payloads = (start..(start + 64).min(TEXT_EVENTS)).map(|index| {
            let mut text = format!("SIMULATED HISTORY {:04} <img src=x onerror=\"globalThis.__historyUnsafe=true\"> & ", index + 1);
            text.push_str(&"x".repeat(TEXT_BYTES - text.len()));
            EventPayload::TextDelta(TextDeltaPayload { stream: TextStream::Assistant, text })
        }).collect();
        run = append(&mut store, &run, payloads)?;
    }
    append(&mut store, &run, vec![state(RunState::Running, RunState::Succeeded)])?;
    let geometry = Geometry { x: 48.0, y: 64.0, width: 460.0, height: 350.0 };
    checked(store.save_layout(&SaveLayoutRequest { contract_version: 2, workspace_id: workspace_id.clone(),
        request_id: id(), expected_revision: 0, layout: Layout { layout_version: 1, workspace_id,
        revision: 0, theme: ThemeName::Dark, active_module: ModuleName::Agents, sidebar_width: 300.0,
        panels: vec![PanelLayout { panel_id: id(), session_id: session.session_id, container_id: None,
            stage: PanelStage::Full, geometry: geometry.clone(), expanded_geometry: geometry }], containers: vec![] } }))?;
    Ok(()) // Drops native exclusive ownership before the coordinator audits/reopens.
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_workbench::execution_v2::ports::JournalReader;
    struct Owned(Lease);
    impl Drop for Owned { fn drop(&mut self) { if let Ok(root) = validate_lease(&self.0) { let _ = fs::remove_dir_all(root); } } }
    #[test]
    fn seeded_history_uses_real_pages_and_is_terminal_after_reopen() {
        let owned = Owned(crate::new_lease().unwrap());
        seed(&owned.0).unwrap();
        assert!(seed(&owned.0).is_err()); // Never reseed existing files.
        let mut store = V2Store::open(&owned.0.root.join("workspace")).unwrap();
        let snapshot = store.bootstrap(WorkspaceSeed { workspace_id: id(), created_at: NOW.into() }).unwrap();
        assert!(store.startup_recover(NOW, &mut Ids).unwrap().committed_events.is_empty());
        assert_eq!(snapshot.sessions.len(), 1);
        let run = &snapshot.runs[0];
        assert_eq!(run.state, RunState::Succeeded);
        assert_eq!(run.last_seq, 604);
        assert!(store.next_queued(&run.workspace_id).unwrap().is_none());
        let mut after = 0; let mut text_count = 0;
        while after < run.last_seq {
            let page = store.list_run_events(&EventPageRequest { contract_version: 2, workspace_id: run.workspace_id.clone(),
                run_id: run.run_id.clone(), after_seq: after, through_seq: Some(run.last_seq), limit: 200 }).unwrap();
            assert!(!page.events.is_empty() && page.events.len() <= 200);
            for event in &page.events {
                after += 1; assert_eq!(event.header().seq, after);
                if let RunEvent::TextDelta { payload, .. } = event { text_count += 1; assert_eq!(payload.text.len(), TEXT_BYTES); }
            }
        }
        assert_eq!(text_count, TEXT_EVENTS);
    }
    #[test]
    fn history_seed_preserves_nonempty_workspace() {
        let owned = Owned(crate::new_lease().unwrap());
        let sentinel = owned.0.root.join("workspace/not-a-fixture.txt");
        fs::write(&sentinel, b"preserve existing input").unwrap();
        assert!(seed(&owned.0).is_err());
        assert_eq!(fs::read(sentinel).unwrap(), b"preserve existing input");
        assert_eq!(fs::read_dir(owned.0.root.join("workspace")).unwrap().count(), 1);
    }
}
