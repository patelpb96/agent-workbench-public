"""Run from repo root: python -m tools.public_release --help."""
from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from . import evaluate, jev, luna, release, scan, news


def read_json(path: str | None, default=None):
    return json.loads(Path(path).read_text(encoding="utf-8")) if path else default


def private_output(path: str, root: Path, value: dict):
    target = Path(path).resolve()
    if target.exists() or target.is_relative_to(root.resolve()):
        raise ValueError("Reports must use new private paths outside release tree")
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(json.dumps(value, indent=2, allow_nan=False), encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser(description="Public export tooling: offline/shadow by default. No automatic promotion.")
    subs = parser.add_subparsers(dest="command", required=True)
    for command in ("scan", "shadow", "luna-propose", "luna-apply", "init", "verify", "push"):
        p = subs.add_parser(command)
        p.add_argument("--root", required=True)
        p.add_argument("--profile", required=True, help="PRIVATE JSON outside source/export trees")
        if command in {"scan", "shadow", "init", "verify", "push"}:
            p.add_argument("--approvals", help="PRIVATE hash-bound binary approval JSON")
        if command in {"scan", "verify"}:
            p.add_argument("--report", required=True)
        if command == "shadow":
            p.add_argument("--out", required=True)
            p.add_argument("--consent-private-data", action="store_true")
            p.add_argument("--budget", type=float, default=0)
        if command == "luna-propose":
            p.add_argument("--paths", required=True, help="PRIVATE JSON array of identified relative paths")
            p.add_argument("--out", required=True)
            p.add_argument("--launcher", required=True)
        if command == "luna-apply":
            p.add_argument("--proposal", required=True)
            p.add_argument("--receipt", required=True)
            p.add_argument("--reviewed-sha256", required=True)
        if command == "init":
            p.add_argument("--previous-public", help="Only a previously verified independent public history")
        if command == "push":
            p.add_argument("--owner", required=True)
            p.add_argument("--repo", required=True)
            p.add_argument("--reviewed-head", required=True)
            p.add_argument("--consent-push", action="store_true")
            p.add_argument("--release-approval", required=True)
    p = subs.add_parser("snapshot")
    p.add_argument("--source", required=True)
    p.add_argument("--ref", required=True)
    p.add_argument("--out", required=True)
    p.add_argument("--report", required=True)
    p.add_argument("--policy", default=str(Path(__file__).with_name("export-policy.json")))
    p = subs.add_parser("evaluate")
    p.add_argument("--gold", required=True)
    p.add_argument("--observations", required=True)
    p.add_argument("--tuning-groups", required=True)
    p.add_argument("--report", required=True)
    p = subs.add_parser("news")
    p.add_argument("--shadow")
    p.add_argument("--evaluation")
    args = parser.parse_args()
    if args.command == "news":
        print(news.render(read_json(args.shadow), read_json(args.evaluation)), end="")
        return 0
    if args.command == "snapshot":
        root = Path(args.out)
        result = release.export(Path(args.source), args.ref, root, read_json(args.policy))
        private_output(args.report, root, result)
        print(json.dumps({"exported": result["exported"], "omitted": len(result["omitted"])}))
        return 0
    if args.command == "evaluate":
        observations = [json.loads(line) for line in Path(args.observations).read_text(encoding="utf-8").splitlines() if line.strip()]
        result = evaluate.evaluate(read_json(args.gold), observations, read_json(args.tuning_groups))
        private_output(args.report, Path(args.gold).parent / "not-a-public-tree", result)
        print(json.dumps({"samples": result["samples"], "eligible": result["eligible_for_additive_routing"], "mode": "shadow"}))
        return 0 if result["eligible_for_additive_routing"] else 1
    root = Path(args.root).resolve(strict=True)
    profile = scan.load_profile(Path(args.profile), (root,))
    approvals = read_json(getattr(args, "approvals", None), {})
    if args.command == "scan":
        result = scan.inspect(root, profile, approvals)
        private_output(args.report, root, result)
        print(json.dumps({"files": len(result["files"]), "findings": len(result["findings"]), "clean": result["clean"]}))
        return 0 if result["clean"] else 1
    if args.command == "shadow":
        print(json.dumps(jev.shadow(root, profile, Path(args.out), consent=args.consent_private_data, budget=args.budget, approvals=approvals)))
        return 0
    if args.command == "luna-propose":
        result = luna.propose(root, read_json(args.paths), profile, Path(args.out), Path(args.launcher))
        print(json.dumps({"candidate_files": len(result["files"]), "status": "proposal_only"}))
        return 0
    if args.command == "luna-apply":
        data = Path(args.proposal).read_bytes()
        print(json.dumps(luna.apply(root, json.loads(data), read_json(args.receipt), reviewed_response_sha256=args.reviewed_sha256, proposal_bytes=data)))
        return 0
    if args.command == "init":
        print(json.dumps({"head": release.init_public(root, profile, approvals, Path(args.previous_public) if args.previous_public else None), "remote": "none", "pushed": False}))
        return 0
    if args.command == "verify":
        result = release.verify_git(root, profile, approvals)
        private_output(args.report, root, result)
        print(json.dumps({"clean": result["clean"], "objects_inspected": result["objects_inspected"]}))
        return 0 if result["clean"] else 1
    if args.command == "push":
        release.push_public(root, args.owner, args.repo, profile, approvals, args.reviewed_head, args.consent_push, read_json(args.release_approval))
        return 0
    return 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (ValueError, RuntimeError, OSError, KeyError, TypeError) as error:
        # Exception text never includes an HTTP body, source text or secret value.
        print("public-release: " + str(error), file=sys.stderr)
        raise SystemExit(2)
