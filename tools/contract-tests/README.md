# Deterministic execution-contract fixtures

These tools define/test **frozen I0 v1**, independently source-reviewed at `4aae305`; they are not a production runner, runtime acceptance, or permission to execute real providers.

## Sources and generated data

- `contracts/fake-scenarios.json`: compact, manually reviewed scenario steps.
- `generate_fixtures.py`: deterministic envelope/UUID/timestamp/snapshot generation using only the Python standard library.
- `contracts/fixtures/fake-*.json` and `commands.json`: generated; do not hand-edit repeated envelopes/examples.
- `contracts/fixtures/layout.json`: small hand-authored layout example.
- `contracts/execution.schema.json`: strict named wire/command/result definitions, authoritative transition matrix and numeric limits; integrator-owned, independently reviewed/frozen I0 v1.
- `generate_interfaces.py`: generates `src-tauri/src/execution/generated.rs` and `frontend/src/execution/generated.ts`, typed event variants, transitions and constants; installed Rust `rustfmt` required. No provider call.
- `src-tauri/src/execution/validation.rs` and `ports.rs`: native semantic checks and compilable synchronous store boundary, not a store/runner implementation.

The fixture UUIDs are predictable v4-shaped test IDs. Never reuse the generator for production identity generation. Fixture timestamps are synthetic, not test execution timestamps.

From the repo root:

```powershell
python tools/contract-tests/generate_fixtures.py
python tools/contract-tests/generate_fixtures.py --check
python tools/contract-tests/generate_interfaces.py
python tools/contract-tests/generate_interfaces.py --check
```

`--check` writes nothing and fails on generated-file drift. Unexpected `fake-*.json` outputs are reported rather than silently deleted. Update compact source, regenerate, and review the diff.

## Validate shapes and invariant matrices

Use `jsonschema==4.26.0`, already pinned in the UI-automation lockfile/venv; no extra dependency installation is needed on this host:

```powershell
.\tools\ui-automation\.venv\Scripts\python.exe -m unittest discover -s tools/contract-tests -v
```

Tests cover schema validity, reproducibility, scenario envelope/state/snapshot consistency, sequence/ID uniqueness, illegal transition/terminal cases, simulated-vs-provider source distinctions, UTF-8 byte limits, recovery, and layout identity/geometry boundaries. Repetitive negative cases use deterministic mutation matrices.

Python reference validators are test-only. Native shared validators now deserialize/replay the same fixtures and test required nulls, disabled providers, canonical arguments, layout ownership, transition/snapshot parity, and atomic truncation+stop. Generated types do not replace semantic validation. Future runtime/store/UI implementations consume these exact seams; passing fixture/type tests does not establish journal/recovery/UI behavior. The native crate's 11 seam tests (including RFC UUID-variant and layout-identity parity regressions) and 4 preexisting shell tests pass under the Windows VS 2022 test command in `docs/acceptance.md`.

No network/model calls, provider subprocesses, screenshots, database migrations, or desktop actions are performed here.
