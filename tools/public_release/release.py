"""History-free export and hash-bound release verification. No private merges."""
from __future__ import annotations

import fnmatch
import json
import os
import re
import subprocess
import unicodedata
from pathlib import Path, PurePosixPath

from .policy import ANONYMOUS_EMAIL, ANONYMOUS_NAME
from .media import clean_raster
from .scan import MAX_FILE_BYTES, decode_text, inspect, sha, text_findings

HARD_DENY = {".git", ".pi", ".claude", ".codex", ".worktrees", "artifacts", "node_modules", "target", "__pycache__"}


def git(root: Path, *args: str, data: bytes | None = None, env: dict | None = None) -> bytes:
    p = subprocess.run(["git", "-C", str(root), *args], input=data, capture_output=True, env=env, timeout=120)
    if p.returncode:
        raise RuntimeError("Git operation failed: " + args[0])  # Don't print URLs/credentials.
    return p.stdout


def safe_path(value: str) -> str:
    path = PurePosixPath(value)
    if not value or path.is_absolute() or "\\" in value or ":" in value or "\0" in value or any(
        part in {"", ".", ".."} or part.endswith((".", " ")) or
        re.fullmatch(r"(?i)(con|prn|aux|nul|com[1-9]|lpt[1-9])(?:\..*)?", part)
        for part in value.split("/")
    ):
        raise ValueError("Unsafe release path")
    return value


def require_isolated_candidate(root: Path) -> None:
    root = root.resolve()
    if any((parent / ".git").exists() for parent in (root, *root.parents)):
        raise ValueError("Candidate must be outside every existing Git checkout")


def allowed(path: str, policy: dict) -> bool:
    parts = PurePosixPath(path).parts
    if any(part in HARD_DENY or part == ".env" or part.startswith(".env.") for part in parts):
        return False
    if path.lower().endswith((".pem", ".key", ".pfx", ".p12", ".sqlite", ".db", ".log", ".zip", ".tar", ".gz", ".exe", ".dll")):
        return False
    return any(fnmatch.fnmatchcase(path, pat) for pat in policy["include"]) and not any(
        fnmatch.fnmatchcase(path, pat) for pat in policy["exclude"])


def export(source: Path, ref: str, target: Path, policy: dict) -> dict:
    source, target = source.resolve(strict=True), target.resolve()
    if target.exists() or target.is_relative_to(source) or source.is_relative_to(target):
        raise ValueError("Export requires a NEW directory physically outside source")
    require_isolated_candidate(target)
    commit = git(source, "rev-parse", "--verify", ref + "^{commit}").decode().strip()
    entries = git(source, "ls-tree", "-rz", "--full-tree", commit).split(b"\0")
    planned, omitted, keys = [], [], set()
    for raw in entries:
        if not raw:
            continue
        meta, path_bytes = raw.split(b"\t", 1)
        mode, kind, oid = meta.decode().split()
        path = safe_path(path_bytes.decode("utf-8", errors="strict"))
        if not allowed(path, policy):
            omitted.append(path)
            continue
        if mode not in {"100644", "100755"} or kind != "blob":
            raise ValueError("Symlinks/submodules cannot be exported")
        dest = safe_path(policy.get("rename", {}).get(path, path))
        if any(part in HARD_DENY for part in PurePosixPath(dest).parts):
            raise ValueError("Protected export destination")
        key = unicodedata.normalize("NFKC", dest).casefold()
        if key in keys:
            raise ValueError("Case/Unicode colliding release paths")
        keys.add(key)
        size = int(git(source, "cat-file", "-s", oid).decode())
        if size > MAX_FILE_BYTES:
            raise ValueError("Oversize export object requires explicit handling")
        planned.append((dest, oid, mode))
    if not planned:
        raise ValueError("Empty export")
    target.mkdir(parents=True, exist_ok=False)
    for dest, oid, mode in planned:
        output = target / dest
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_bytes(git(source, "cat-file", "blob", oid))
        if mode == "100755":
            output.chmod(0o755)
    # Provenance is returned to the PRIVATE caller, never embedded in public tree.
    return {"source_commit_private": commit, "exported": len(planned), "omitted": omitted}


def anonymous_env() -> dict:
    env = dict(os.environ)
    for key in list(env):
        if key.startswith("GIT_"):
            del env[key]
    env.update({"GIT_AUTHOR_NAME": ANONYMOUS_NAME, "GIT_COMMITTER_NAME": ANONYMOUS_NAME,
                "GIT_AUTHOR_EMAIL": ANONYMOUS_EMAIL, "GIT_COMMITTER_EMAIL": ANONYMOUS_EMAIL,
                "GIT_AUTHOR_DATE": "2000-01-01T00:00:00+00:00", "GIT_COMMITTER_DATE": "2000-01-01T00:00:00+00:00",
                "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull})
    return env


def init_public(root: Path, profile: dict, approvals: dict | None = None, previous: Path | None = None) -> str:
    require_isolated_candidate(root)
    parent = None
    if previous is not None:
        check = verify_git(previous, profile, approvals)
        if not check["clean"]:
            raise ValueError("Previous public history has privacy findings")
        parent = check["head"]
    report = inspect(root, profile, approvals)
    if not report["clean"]:
        raise ValueError("Privacy findings must be resolved before anonymous commit")
    env = anonymous_env()
    git(root, "-c", "init.templateDir=", "init", "--initial-branch=public", env=env)
    for key, value in {"user.name": ANONYMOUS_NAME, "user.email": ANONYMOUS_EMAIL,
                       "commit.gpgsign": "false", "core.logAllRefUpdates": "false",
                       "core.autocrlf": "false", "push.followTags": "false"}.items():
        git(root, "config", "--local", key, value, env=env)
    git(root, "-c", "core.hooksPath=", "add", "--all", env=env)
    if parent is None:
        git(root, "-c", "core.hooksPath=", "commit", "--no-gpg-sign", "-m", "Agent Workbench public snapshot", env=env)
    else:
        # Only audited PUBLIC history crosses this boundary. No private dev
        # branch, shared object store, remotes, tags or author metadata is copied.
        git(root, "-c", "core.hooksPath=", "fetch", "--no-tags", "--no-write-fetch-head", str(previous.resolve()), "refs/heads/public", env=env)
        tree = git(root, "write-tree", env=env).decode().strip()
        commit = git(root, "-c", "commit.gpgsign=false", "commit-tree", tree, "-p", parent,
                     data=b"Agent Workbench public snapshot\n", env=env).decode().strip()
        git(root, "update-ref", "refs/heads/public", commit, env=env)
    return git(root, "rev-parse", "HEAD", env=env).decode().strip()


def verify_git(root: Path, profile: dict, approvals: dict | None = None,
               approved_owner: str | None = None) -> dict:
    """Verify history and metadata. `approved_owner` is the one name allowed in the
    remote URL after the owner has accepted attribution; it is redacted before the
    metadata scan so any OTHER mention of that name is still a finding."""
    root = root.resolve(strict=True)
    if not (root / ".git").is_dir() or (root / ".git/commondir").exists():
        raise ValueError("Public repo must have an independent Git object directory")
    if (root / ".git/objects/info/alternates").exists():
        raise ValueError("Git object alternates forbidden")
    refs = git(root, "for-each-ref", "--format=%(refname)").decode().splitlines()
    if refs != ["refs/heads/public"]:
        raise ValueError("Only the public branch may exist in publishing repository")
    if git(root, "status", "--porcelain", "--untracked-files=all").strip():
        raise ValueError("Publishing repository is dirty")
    report = inspect(root, profile, approvals)
    findings = list(report["findings"])
    objects = git(root, "rev-list", "--objects", "--all").decode().splitlines()
    root_count = 0
    inspected = 0
    # Tree paths from every reachable commit, not only HEAD's filenames.
    commits = git(root, "rev-list", "--all").decode().splitlines()
    for commit in commits:
        for raw in git(root, "ls-tree", "-rz", "--full-tree", commit).split(b"\0"):
            if not raw:
                continue
            meta, path = raw.split(b"\t", 1)
            mode, kind, oid = meta.decode().split()
            name = safe_path(path.decode("utf-8"))
            findings.extend(text_findings("git-tree", name, profile))
            if mode not in {"100644", "100755"} or kind != "blob":
                raise ValueError("Non-regular public Git object")
    approved_hashes = {a["sha256"] for a in (approvals or {}).values()
                       if a.get("reviewed") is True and a.get("metadata_stripped") is True and a.get("ocr_clean") is True}
    for entry in objects:
        oid = entry.split(" ", 1)[0]
        kind = git(root, "cat-file", "-t", oid).decode().strip()
        if kind not in {"commit", "tree", "blob"}:
            raise ValueError("Unexpected reachable Git object")
        if kind == "tree":
            continue
        size = int(git(root, "cat-file", "-s", oid).decode())
        if size > MAX_FILE_BYTES:
            raise ValueError("Oversize historical object")
        data = git(root, "cat-file", kind, oid)
        if kind == "commit":
            text = data.decode("utf-8", errors="strict")
            headers, message = text.split("\n\n", 1)
            parents = re.findall(r"^parent ", headers, re.M)
            root_count += not parents
            if len(parents) > 1:
                raise ValueError("Private merges are never publication updates")
            if "gpgsig " in headers or "mergetag " in headers:
                raise ValueError("Personal signing/merge metadata forbidden")
            for field in ("author", "committer"):
                expected = f"{field} {ANONYMOUS_NAME} <{ANONYMOUS_EMAIL}> 946684800 +0000"
                if expected not in headers.splitlines():
                    raise ValueError("Non-anonymous author/committer or timestamp")
            if message.strip() != "Agent Workbench public snapshot":
                raise ValueError("Unexpected public commit message")
        else:
            try:
                text = decode_text(data)
            except (UnicodeError, ValueError):
                if sha(data) not in approved_hashes or not any(clean_raster(a, data) for a in (approvals or {})):
                    raise ValueError("Unreviewed historical binary")
                continue
        findings.extend(text_findings("git-object/" + kind, text, profile))
        inspected += 1
    if root_count != 1:
        raise ValueError("Public history must have exactly one independent root")
    # Remote URLs, local repo config and hook files are private operational data,
    # but verify them too so a personal remote cannot be used accidentally.
    metadata = "\n".join((git(root, "remote", "-v").decode(),
                            git(root, "config", "--local", "--list").decode()))
    if approved_owner:
        metadata = metadata.replace(f"https://github.com/{approved_owner}/", "https://github.com/<approved-owner>/")
    findings.extend(text_findings("git-metadata", metadata, profile))
    return {"clean": not findings, "findings": findings, "manifest": report["manifest"],
            "head": git(root, "rev-parse", "HEAD").decode().strip(), "objects_inspected": inspected}


def push_public(root: Path, owner: str, repo: str, profile: dict, approvals: dict,
                reviewed_head: str, consent: bool = False, release_approval: dict | None = None,
                identifiable_owner: bool = False) -> None:
    if not consent or not re.fullmatch(r"[A-Za-z0-9-]{1,39}", owner) or not re.fullmatch(r"[A-Za-z0-9_.-]{1,100}", repo):
        raise ValueError("Explicit owner/repository and push consent required")
    if text_findings("owner", owner, profile):
        # Ownership was anonymous by default. An identifiable owner is allowed only as a
        # deliberate, recorded choice, never as a silent side effect of a refactor.
        if not identifiable_owner:
            raise ValueError("Identifiable remote owner: pass --owner-is-identifiable to accept attribution")
    expected = f"https://github.com/{owner}/{repo}.git"
    urls = git(root, "remote", "get-url", "--all", "--push", "origin").decode().splitlines()
    fetch_urls = git(root, "remote", "get-url", "--all", "origin").decode().splitlines()
    if urls != [expected] or fetch_urls != [expected]:
        raise ValueError("Exactly one explicit anonymous HTTPS origin is required")
    check = verify_git(root, profile, approvals, approved_owner=owner if identifiable_owner else None)
    if not check["clean"] or check["head"] != reviewed_head:
        raise ValueError("Review is missing, stale, or has privacy findings")
    approval = release_approval or {}
    if (approval.get("head") != reviewed_head or approval.get("manifest") != check["manifest"] or
            approval.get("profile_sha256") != sha(json.dumps(profile, sort_keys=True)) or
            approval.get("independent_review") != "APPROVE" or not approval.get("reviewer") or
            approval.get("tests") != {"privacy": "PASS", "frontend": "PASS", "native": "PASS"}):
        raise ValueError("Exact private release approval with independent review and test evidence required")
    # Verify the authenticating account, not only the URL. No gh repo creation or
    # push under an identifiable account is attempted by default.
    p = subprocess.run(["gh", "api", "user", "--jq", ".login"], capture_output=True, text=True, timeout=30)
    if p.returncode or p.stdout.strip() != owner:
        raise ValueError("Dedicated pseudonymous GitHub authentication required")
    config = git(root, "config", "--show-origin", "--get-regexp", r"^(url\..*\.(insteadOf|pushInsteadOf)|remote\..*\.(mirror|push)|push\.followTags)$").decode()
    if any("insteadof" in line.lower() or "pushinsteadof" in line.lower() or "mirror" in line.lower() for line in config.splitlines()):
        raise ValueError("Git URL rewrites and mirror settings forbidden")
    git(root, "-c", "push.followTags=false", "-c", "core.hooksPath=", "-c", "credential.helper=",
        "-c", "credential.helper=!gh auth git-credential", "push", "origin", reviewed_head + ":refs/heads/public",
        env=anonymous_env())
