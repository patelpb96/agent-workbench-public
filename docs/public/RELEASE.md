# Private development → public snapshots

## Threat model and boundaries

Public readers can inspect every pushed commit/blob/path, metadata, lockfile,
image pixel and decoded string. Protect private names/handles, phone numbers,
home paths, host/LAN/device identifiers, credentials and private conversations.
Emails are explicitly permitted, including identifying local parts: **unlinkability
is not promised**. GitHub and network providers still know the authenticated
account/IP. An identifiable hosting owner is not made anonymous by a generic commit.

The exporter reads an exact **committed ref**, not dirty working files, through
`git ls-tree`/`cat-file` and a reviewed allowlist. It never switches the developer's
branch, rewrites history or closes sessions. Historical reviews/handoffs, agent
configuration, backup tools, runtime state and unknown archives are not exported.
Review the private omission report; omitted does not mean inspected or harmless.

Export into a fresh directory **outside every Git checkout**, with a separate
object store. Do not copy `.git`, push a worktree's private ancestry, merge `dev`
into `public`, publish private review packets, or use force/mirror/all/tags pushes.

## One-time private setup

Create an identity profile OUTSIDE both source and export trees. Restrict its
filesystem permissions to the owner. Do not commit it, gold labels, model payloads,
reports, receipts, exceptions or media approvals. Collect known names/variants,
OS/hosting usernames, hostname/private domains and known phone numbers; exact
identifiers supplement broad pattern scans. The source cannot discover what the
owner has not supplied. Profile format:

```json
{"identifiers":["PRIVATE_NAME","PRIVATE_HANDLE","PRIVATE_HOST"],"phones":[],"email_allowed":true}
```

Use an independent pseudonymous hosting account/organization and its dedicated
credentials if ownership anonymity matters. This tool will not create a public repo
under a personal account and describe it as anonymous.

## Private automatic staging, public manual release

The private source may refresh a separate `release-private` branch after pushes to
its configured development branch (currently `main`). That automation requires a
bound private repository, offline checks and hash-bound media approval; it fails
closed and keeps the prior stage on findings. It does not make live model calls.
Workflow files stay on the source branch, not in the automatic snapshot.

**The public version changes only after an explicit user request.** Private-stage
success is neither public publication nor native-release certification. Never
change hosting visibility or merge private source/staging history into `public`.
Use the independent public-history procedure below when publication is requested.

## Repeatable release

The following is a template: replace the neutral paths with private local paths.
Keep the private source remote separate. Use `dev` as the normal development branch
if desired; the exporter accepts any exact reviewed committed ref without renaming
or modifying existing branches.

```sh
# 1. Freeze/review the dev checkpoint; leave unrelated dirty work alone.
python -m tools.public_release snapshot --source /workspace/private-app --ref dev --out /workspace/candidate-new --report /workspace/private-reports/export.json

# 2. Deterministic scan of ALL exported paths and contents. Nonzero means blocked.
python -m tools.public_release scan --root /workspace/candidate-new --profile /private/identity.json --report /workspace/private-reports/scan.json

# 3. Offline shadow by default: records coverage, never fabricates probabilities.
python -m tools.public_release shadow --root /workspace/candidate-new --profile /private/identity.json --out /workspace/private-reports/shadow-new
```

A release snapshot must include this tooling and the public README/media sources.
Use the private preparation branch to review tooling changes before exporting.
The public README is supplied by `PUBLIC_README.md` (renamed to `README.md`);
private `README.md` and historical operational docs are not copied.

### Jev: opt-in shadow only

Before live testing, obtain explicit owner consent for sending private source lines
to TypeSafe and a dollar cap. Only then set `TYPESAFE_API_KEY` and add
`--consent-private-data --budget <approved-cap>`. No key is put on a command line.
Requests go only to the fixed HTTPS TypeSafe endpoint; redirects fail closed.

Every text line is assigned stable file/line/content/policy hashes in deterministic
order. Small batches share adjacent context; probabilities come from four independent
Nouls. Credential-shaped documents and long lines are withheld, not silently called
clean. Exact secret-masked requests and results remain PRIVATE. Reservations charge
before sending; errors retain their conservative reservation. Current token pricing
is pinned in code and must be rechecked before an authorized run.

Read [the routing news](AGENT_NEWS.md). Shadow observations **never decide** edits,
clear regex findings, bypass review or publish. Probability >=0.15 only proposes a
Luna/human review. Offline/error/malformed results are explicitly unmeasured.

### Honest 99% evaluation

Store independent human-reviewed labels outside Git. Each gold row has `id`,
`text`, all four boolean `labels`, `source_group`, `family`, `split: holdout`,
`reviewed_by`, `independent: true`, and `gold_origin: human_real|human_synthetic`.
Observation IDs and line hashes must match. Supply the frozen tuning-group list.

```sh
python -m tools.public_release evaluate --gold /private/gold.json --observations /private/shadow/observations.jsonl --tuning-groups /private/tuning-groups.json --report /private/evaluation.json
```

A one-sided 95% Wilson lower confidence bound must be >=99% for overall accuracy
and each risk's recall. Require >=500 distinct nontrivial held-out samples, >=300
positive examples per category, zero missed positive lines, no tuning-group leakage,
no missing/mock/stale/error evidence. Exact/near-template duplicates do not pad the
sample count. Human provenance and representative sampling cannot be proven by a
boolean JSON field: an independent reviewer must inspect them. Never tune on the
holdout. Changing the model/questions/threshold invalidates prior evidence.

The evaluator reports **eligibility**, never automatically promotes Jev. Even an
approved promotion only adds review routing; deterministic checks remain mandatory.
Regenerate shared news with `python -m tools.public_release news --shadow
/private/shadow/summary.json --evaluation /private/evaluation.json` (or `news.render`)
from sanitized summaries after each approved policy/evaluation update. Review the
rendered Markdown and update the shared file; never paste raw findings.

### Luna 6/max scrub phase

Use identified candidate paths only:

```sh
python -m tools.public_release luna-propose --root /workspace/candidate-new --profile /private/identity.json --paths /private/candidates.json --out /private/luna-new --launcher /path/to/pi-launcher.js
```

Luna has no tools/MCP/session/context discovery and runs with thinking `max`.
Secret-shaped files are withheld. The broker supplies numbered lines and hashes;
Luna returns non-overlapping line ranges plus replacement text. Independent Argon
review must approve the exact persisted response SHA-256 and its actual diff.
Only then use `luna-apply` with that hash, proposal and receipt. The broker validates
all preconditions before writing, refuses Git checkouts/path escapes/stale files,
and checks non-candidate file hashes. Model output is never evaluated or executed.
A partial write/crash is an invalid candidate, not a release.

Use portable environment lookup in production, not a literal placeholder path.
Synthetic fixtures may use `USERPROFILE`; preserve fixture intent and Unicode.
Never weaken executable pins, permission boundaries or tests to make a scrub pass.
Repeat the full scan and affected tests after applying. Unknown cases go to review.

### Screenshots

`tools/public_release/screenshots.mjs` renders the actual interface with isolated,
synthetic in-page fixtures in headless Chromium. It does not capture the desktop or
run real terminal/provider commands. Run it after locked frontend dependencies and
the project-local Playwright browser are installed. `collage.py` (Pillow required)
re-encodes fresh RGB pixels, supports solid-rectangle redactions **before** scaling,
and labels crops and simulation accurately. Never use reversible blur/alpha masks.

Run local OCR on final full-resolution images (Windows: `ocr-windows.ps1`), scan its
text and manually inspect every final image. Approve only exact SHA-256 hashes.
The private media approval map is keyed by relative path with `sha256`, `reviewed`,
`metadata_stripped`, `ocr_clean` all true. Supported binaries are strict metadata-free
PNG and PNG-frame ICO only; arbitrary binaries cannot be approved by a boolean.
OCR is imperfect, so absence of text hits is not proof that every pixel is safe.

### Commit and publish

Run project checks on the actual sanitized tree. Exclude generated builds/deps from
the final candidate; never ship executables, databases, source maps or transcripts.
Scan again with private hash-bound media approvals. The first `init` creates one
anonymous root; for updates use `init --previous-public <verified-public-checkout>`.
Only audited **public** ancestry is transferred; no private parent is merged.

```sh
python -m tools.public_release init --root /workspace/candidate-new --profile /private/identity.json --approvals /private/media.json
python -m tools.public_release verify --root /workspace/candidate-new --profile /private/identity.json --approvals /private/media.json --report /private/final-git.json
```

Verification checks all reachable commit identities/messages, historical blobs and
filenames, refs, remotes, binary approvals and current tree. Author AND committer
are generic; personal signing is disabled; publication timestamps are normalized.
No tags, shared object store, private merges, unexpected refs or URL rewrites.

Set exactly one origin to the approved HTTPS repository only after ownership and
authentication are resolved. A remote owner that matches the identity profile is
refused unless `--owner-is-identifiable` is passed: attribution is a deliberate,
recorded choice, never a side effect of a refactor. `push` requires explicit consent,
matching GitHub authentication, reviewed HEAD and `--release-approval <private-json>`. That
private approval binds `head`, `manifest`, `profile_sha256`, an actual `reviewer`,
`independent_review: APPROVE`, and `tests: {privacy: PASS, frontend: PASS, native: PASS}`.
The profile digest is SHA-256 of `json.dumps(profile, sort_keys=True)`. Attach real
review/test provenance; do not manufacture these fields. The tool binds bytes and
status but cannot prove that a human/reviewer actually performed a review.
It sends only the reviewed commit to `refs/heads/public`, using the same dedicated
`gh` credential helper verified for the owner. Human/platform-level anonymity
remains outside Git.

## Coverage limits / stop conditions

Known-pattern/limited recursive decoders are conservative sensors, not a proof of
absence of all personal information. Novel encodings, contextual private facts,
third-party metadata, OCR misses, malicious tools and concurrent filesystem changes
remain review concerns. Never claim "all personal data removed" from regexes alone.

Stop on any unhandled file, missing identity profile, unexpected source change,
malformed model response, stale hash, failed tests, unresolved visual check,
uncertain ownership or missing pseudonymous remote/credentials. Preserve artifacts;
do not delete evidence, rewrite private history or close live sessions to recover.
