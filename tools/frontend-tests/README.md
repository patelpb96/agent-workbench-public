# Execution modules and transcript markup tests

From the repository root, after installing the pinned frontend dependencies:

```text
node tools/frontend-tests/run.mjs
```

The runner uses the locally installed TypeScript compiler with strict checking and `--noEmitOnError`, emits generated/client/reconciliation, frontend workspace ports, controller/layout and compile-time API checks as CommonJS modules into ignored `artifacts/frontend-contract-src/`, then runs sorted source `execution/*.test.cjs` and local `tools/frontend-tests/*.test.mjs` suites in one Node invocation. Only one runner/compiler may write the shared outputs at a time. Output is outside the frontend `type: module` package. Compiler failure stops before any possibly stale test output is loaded.

`transcript.test.mjs` compiles the actual `SessionTranscript.svelte` to SSR, bundles its real helpers and Svelte server renderer using the already locked frontend Svelte/esbuild installation, and writes only ignored `artifacts/frontend-component-tests/`. It checks stable rendered markup when refresh toggles, accessible busy state, terminal fixtures, earlier-history controls and escaped display text. It does not use a substitute transcript implementation or add dependencies.

The Tauri adapter is separately type-checked by the frontend build; pure suites do not import it. Injected transports, listeners, clocks/timers and fixture pages are simulated. These tests do not launch a browser, Workbench, provider or network request. SSR markup assertions do not measure client DOM identity, layout pixels, WebView/GPU flicker or live subscription/desktop acceptance. Native runtime/SQLite coverage is separate; see [parent integration evidence](../../docs/reviews/s1-integration.md).
