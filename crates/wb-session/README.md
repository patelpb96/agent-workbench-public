# wb-session

Persistent-session broker, host, client and CLI for Agent Workbench. Design: `docs/design/session-daemon.md`; spike evidence: `docs/design/spike-session-host-results.md`.

- `wb-daemon` (broker): single-instance named-pipe server, auth, capability tiers, grants, admin flow, session registry and re-adoption.
- `wb-host`: one per terminal session; owns the ConPTY shell, a KILL_ON_JOB_CLOSE Job and an output ring.
- `wb`: CLI for people, scripts and agents (`ls`, `new`, `attach`, `read`, `send`, `end`, `watch`, `wait`, `log`, `grant`, `grants`, `revoke`, `draft`, `accept`, `discard`, `propose`, `type`), with `--json` and stable exit codes: `docs/design/wb-automation.md`. It lives in `src/cli.rs`; `src/bin/wb.rs` only calls it. `wb attach ID` uses a Workbench terminal from a normal console (raw VT input and output, replay of the history or `--fresh`, size follows your window, **Ctrl+] detaches** and the terminal keeps running); the GUI panes are the other way in.
- Output logging (`log` module, `wb log ID start|stop|status`): `log start` makes the session host append the terminal's raw output to a file the **service** names, `<data folder>/logs/<terminal id>-<UTC yyyymmddThhmmssZ>.log`, created with the same user-only protection as the data folder. It begins with the next byte the terminal prints (not with the scrollback), keeps recording when the GUI or CLI that started it disconnects, stops at 64 MiB or on the first write error (the answer says which), and stops when the terminal ends — the file stays. Because it writes terminal bytes to disk it is **opt-in per session and per request**, never a default and never a client-chosen path. Only people may do it: an agent is refused with `forbidden` even while it holds a grant. Recording lives in the host (`OP_LOG_START`/`OP_LOG_STOP`/`OP_LOG_STATUS`), the only place that sees every output byte exactly once; the broker decides the file, and the audit log records start/stop with ids only, never content.
- Pure logic with unit tests: `caps` (tiers, origin, grants, focus gate, staging, admin flow), `proto` (JSON frames, base64), `auth` (HMAC challenge-response),
  `frames` (broker-to-host frames: closed / truncated / too-large are different outcomes; bounds-checked body parsers), `ring` (the bounded output history with
  absolute offsets), `log` (the log writer: cap, write-error stop, file naming), `sync` (recovering locks: a poisoned lock is repaired or fails closed, never a panic cascade).
- Hardening rules (see `docs/design/hardening-plan.md`): no `unwrap`/`expect` or truncating cast in production code (the strict clippy step in
  `node tools/check-all.mjs` enforces it), no data lock held across blocking I/O (the per-session connection lock is the one documented exception), no
  unbounded channel or uncapped buffer, a host never panics on anything a peer sends, every new connection counts against `MAX_CONNECTIONS`.
- `tests/adversarial.rs` is the regression suite for the defects found by the Step 4 review; add new confirmed defects there test-first.

## Build and test

```
cargo-vs.cmd build --offline --target x86_64-pc-windows-msvc
cargo-vs.cmd test  --offline --target x86_64-pc-windows-msvc --lib      # unit tests
cargo-vs.cmd test  --offline --target x86_64-pc-windows-msvc --no-run   # build the end-to-end tests
target\x86_64-pc-windows-msvc\debug\deps\broker-<hash>.exe              # run them
```

**Run the end-to-end tests from the compiled exe, not through `cargo test`.** They start a real broker that launches detached hosts with `CREATE_BREAKAWAY_FROM_JOB`. If the invoking process tree sits inside a Job that forbids breakaway (some agent harnesses and CI runners do; the tool this crate was developed with does when started through `cargo test`), the broker correctly refuses to create a Persistent session (`host_launch_failed`: "the broker runs inside a Job that forbids breakaway"). That is design finding 3 observed live, not a bug.

`tests/soak.rs` repeatedly connects, creates, floods, drops the connection without ending, ends and fails handshakes for about 25 s, then checks the broker's handle count and private memory stayed flat, no host was left behind and the broker still answers. For an overnight run, start the compiled exe by hand with the length in seconds: `set WB_SOAK_SECS=28800` then `target\x86_64-pc-windows-msvc\debug\deps\soak-<hash>.exe --nocapture`.

Tests use their own data directory and pipe name (`wbtest-*`) and never touch a real install. The `Daemon` guard kills any daemon a failing test started.

## Output logging (opt-in)

`wb log ID start` (or the GUI's future pane menu) makes that terminal's host append every output byte to
`<data folder>/logs/<id>-<UTC yyyymmddThhmmssZ>.log`, created with a user-only ACL like the data folder itself. Recording
starts at the moment of the request (the ring's older scrollback stays in memory only), survives the client that started
it disconnecting, stops at 64 MiB or on the first write error (the stop reason is reported), and stops when the terminal
ends while the file stays on disk. Starting twice keeps one file; stopping when nothing is recording is not an error.
Only people start logs (an agent gets `forbidden`, grant or no grant) and only the service chooses the path.
`tests/log.rs` covers start/emit/stop with an exact byte comparison, the disconnect case, the agent refusal, the
status after the terminal ends and the CLI; the writer's cap and write-error stops are unit-tested in `src/log.rs`
with a small cap, so no production knob exists for it.

## What a terminal's shell gets

A new terminal starts in the user's folder (`%USERPROFILE%`), not in whatever folder the app or service was started from (that was `C:\Windows\System32` when launched from Explorer). The folder holding `wb.exe` is put first on its `PATH`, so `wb ls` and `wb attach` work inside it, and `WB_TERMINAL` holds the terminal's own id. The shell runs as the standard user, never elevated. `tools/build-release.mjs` installs a new `wb-host.exe` beside a running one (a running executable can be renamed), so terminals started afterwards use it without stopping existing ones.
