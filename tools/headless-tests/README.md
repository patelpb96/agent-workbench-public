# Non-intrusive headless workflow integration tests

This is the default UI workflow test path. **No desktop app, visible browser, physical mouse/keyboard input, foreground activation, personal browser profile or production app-data is used.** Compilation still consumes normal CPU/disk resources.

## Setup and run

Install the locked frontend dependencies (`npm --prefix frontend ci`), then once:

```text
node tools/headless-tests/install-browser.mjs
```

The pinned Playwright **1.64.1** Chromium download stays in ignored `artifacts/playwright-browsers/`; it does not configure the user's browser or change global environment/settings. Standard Windows VS2022/Rust prerequisites are the same as native tests.

Run from the repository root:

```text
node tools/headless-tests/run.mjs
```

Or `npm --prefix frontend run test:headless`. The runner accepts no headed/debug/browser arguments, refuses `PWDEBUG` mode, launches only bundled headless Chromium with a fresh test context, binds a test Vite server to loopback, and closes it after the one-shot run. Cargo target writers must remain serialized; do not overlap this runner's build with another Cargo writer. The Vite test entry and shell facade are outside production frontend entry points and are never imported by the normal application build.

## What is real vs simulated

- **Real:** complete Svelte `App.svelte` and components/styles; execution client, controller, layout/reconciliation modules and Tauri JS invoke/listener calls; real Rust `Coordinator::submit`, strict request decoding/output checks, mailbox/runtime scheduling/fake runner, real SQLite transactions, startup recovery and durable history/layout.
- **Test transport:** Playwright's private page binding forwards only the seven frozen execution commands to an owned Rust integration-test process over framed JSON stdio. Native notifications are forwarded/coalesced into the official Tauri JS mock event plumbing. There is no HTTP execution API or test hook in the production application.
- **Explicit stubs:** Tauri runtime-info/window visibility/appearance/file-picker plumbing. These do **not** establish WebView2 IPC/capabilities, actual OS window controls, picker/media/transparency behavior or clean-machine packaging acceptance.
- **Browser isolation:** fresh headless context; page network requests restricted to the owned loopback test origin, no real provider calls, user cookies or browser executable/profile reuse. This is trusted developer test code, not an OS sandbox against a malicious same-user process.

## Native test-process isolation

`src-tauri/tests/headless_bridge.rs` is an **ignored integration-test entry**, excluded from the desktop executable. The runner builds that exact Cargo test artifact; it never launches `agent-workbench.exe`. Even direct `cargo test -- --ignored` does not serve without the explicit headless-run environment flag.

The native fixture atomically creates a fresh UUID directory under the OS temp directory, with an ownership marker/nonce and private `workspace/`. Restart accepts only a matching isolated lease; browser calls cannot choose a filesystem path, lease, process or shutdown operation. The same production coordinator handles all execution requests. JSON frame/request/reply queues and hint retention are bounded; there is no substitute JavaScript execution engine.

EOF performs orderly coordinator shutdown without fabricating a terminal state. For orderly stops the runner verifies exit0. After any confirmed child exit (including a crash), it attempts to remove only this invocation-created matching canonical lease—not a prefix/glob sweep—while still reporting the original failure. Unconfirmed child termination never permits directory removal. A hung owned **test child only** can be terminated after the bounded shutdown timeout; a production Workbench process is never targeted. Unsafe cleanup fails the test rather than deleting an unverified path.

## Covered scenarios and evidence

The DOM workflow test drives create/open without dispatch; success; startup failure; wait/cancel; old-run selection; keyboard layout movement and explicit save; real native process shutdown/restart with `running -> interrupted`; identical saved layout/history; no additional fake starts across reload/poll. A second test verifies exact native rejection codes for both real providers, malformed-call survival and browser-command/path scope. A third intentionally crashes only its owned headless test child and verifies that cleanup removes the matching workspace while preserving the failure result. This test is not a production-app crash/power-loss certification.

Focused layout cases now save/restart each full → compact → block → full stage, preserving IDs and expanded geometry; root nesting/unnesting also round-trips through the actual selector and native SQLite. Assertions compare rendered canvas-relative x/y/width/height, exact durable layouts and read-only reload/save counts. Both cases create zero runs and use separate isolated workspaces. The unclamped fixture intentionally does not close the deferred C4 clamped-panel shrink issue or certify the full nesting rejection/native geometry matrix.

Three additional cases cover post-commit lost replies for `start_fake_run` and `save_layout`, and a genuine SQLite layout revision conflict. A trusted Node-only `TransportFaults` fixture normally passes through unchanged; it can lose one successful native reply or issue one different native save before forwarding the stale UI request. It never fabricates an acknowledgement, receipt, conflict error or authoritative snapshot. The competing writer is a coordinated second command to the same real coordinator, not a second OS process/storage owner. Page commands cannot arm faults. Full captured DTOs stay private to the synthetic test; attached evidence contains only mode/counters/request IDs/replay flags/error codes. Capture is capped at eight tracked calls and one armed fault per test.

Tests verify exact original DTO replay despite changed composer inputs/later layout edits, a real `replayed: true` receipt, identical terminal history/events, no automatic retry across refresh/poll, later intent requiring a distinct explicit save, and conflict -> refresh -> explicit reapply (no write) -> explicit save. Layout retry/conflict results persist across native restart. Six pure Node mock-adapter tests validate the gate itself (not native durability), including the create/cancel targets and refusal of read-only commands; the default runner executes these before compiling/running the browser/native suite.

Paging uses an optional fixed **native history fixture**, selected only by a trusted Node boolean. On the first fresh validated lease, Rust emits its lease receipt before fixture work (so the parent can safely clean a seed failure), then uses production `JournalStore` operations/shared validation to create one terminal 604-event run with 600 ×8 KiB text payloads and a saved panel. No raw SQL or alternative execution engine; this seeds journal data, not a timed fake-runner stream. Nonempty workspaces, resumed seeding and unknown environment flags are rejected; inherited seed flags are removed, and restart never reseeds. Three native tests exercise gating, nonempty preservation and real page/reopen behavior.

The browser paging case compares real SQLite event data with DOM text/sequence windows, walks to event1 without gaps/duplicate coverage, verifies UTF-8 text/event bounds and HTML escaping, preserves history position across Refresh, returns to tail, and reopens the same history/layout without UI mutations. The visibility case explicitly shadows document visibility only in its isolated page and dispatches the real event handler; native window visibility remains a stub, not OS acceptance. Two hidden windows each observe five real 460ms browser timer ticks while frontend reads stay fixed; a real native cancellation occurs while hidden, then resume reconciles it, resumes polling and never redispatches. Original document properties are restored. This does not test actual OS minimize/tab visibility.

Eleven geometry regressions now cover clamped panel keyboard/virtual-pointer move and one-axis resize, small-container nesting/unnesting, and the analogous root-container moves/resizes. Virtual viewport changes and CDP pointer actions affect only headless test pages. Scene operations carry explicit move/resize/nest intent: passive clamping never writes; move/nest preserves canonical sizes; explicit resize uses a hybrid origin (visible x/y plus stored dimensions) and persists requested dimensions, while rendered bounds and committed position remain clamped. Deliberate width/height changes, expanded panel geometry, exact SQLite save/restart and reopening a wider viewport are checked. Unshifted one-axis resize preserves the other axis; existing snap rules remain unchanged. Two crossing-threshold pointer-resize cases additionally ensure a clamped panel/root container does not jump from x0 to x20 when its requested width becomes smaller than the viewport. Original ten browser test bodies remain intact; the shared reload helper now checks the saved panel count (including empty container-only scenes).

Lost-reply coverage also includes `create_session` and `cancel_run`, completing focused browser coverage for all four mutation commands. Creation replay retains its original label/UUID despite later form edits, never duplicates the session or dispatches a run, then explicit roster open works. Cancellation replay retains its original run/UUID even after selecting different completed history; terminal snapshots/events stay identical and the selected historical run remains selected. Both cases prove Refresh/one visible polling interval does not retry automatically. Fault arming remains trusted Node-only; this does not broaden the seven-command browser transport or certify arbitrary disconnect/crash timing.

The connected Tasks board also runs against native snapshots: active/FIFO queued/history groups and session filters, exact historical Inspect, existing block/compact panel reveal, absent-panel staging, explicit-save-only geometry, restart interruptions/no redispatch, escaped labels and no private prompt display. Its component SSR suite covers stale/empty/unloaded and quiet-refresh distinctions separately. `headless-run-board-preview` is a synthetic fresh-headless-context image, never a desktop screenshot. No task-manifest watcher, scheduler, real provider or approval action is enabled.

Models & Usage also projects existing native summaries, preserving unknown values, filtering sessions and reusing exact read-only Inspect. Its real-native fake runs stay null-usage across restart; separate actual-component SSR tests exercise synthetic reported/estimated/partial/zero counters and exact 200×maximum-safe-integer aggregation. These are not provider billing, pricing, catalog or quota evidence. The synthetic `headless-usage-preview` screenshot never captures the physical desktop.

Events & Approvals projects only selected-run event header metadata, not payloads or genuine approval authority. Exact headers/identity isolation and shared earlier/live paging are checked against native pages, including all 604 fixture events, unchanged text-budget-driven tails, omissions, read-only selection/Refresh and restart. The UI-only projector has separate synthetic identity/cap/privacy/SSR tests. No new event store, per-run raw cache, protocol command or approval action. `headless-event-metadata-preview` is a synthetic headless capture only.

Workspace Diagnostics completes the bounded sidebar projections: allowlisted connection/count/layout/history/unresolved-command UUID metadata, no raw logs/keys/bodies/provider details. Actual SSR covers privacy/caps/0-vs-unavailable/stale/quiet read-pump updates. Native-browser cases cover live queue counts/staged-vs-saved intent/restart, plus a lost committed reply that remains unresolved through Refresh and clears only after explicit inspector retry/native replay. It does not add new probes, mutate authority or measure backend process/resource health. The diagnostic screenshot is synthetic/headless only.

JSON results, per-test native startup/normal-exit/owned-cleanup counters, failure screenshots/traces and compiler records stay under ignored `artifacts/headless-tests/`. These screenshots are isolated browser renderings, not desktop captures. Neither a test's existence nor mocked shell operations imply native OS acceptance. Separate pure module/native suites remain useful and should also pass.
