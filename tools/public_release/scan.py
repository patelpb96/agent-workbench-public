"""Conservative deterministic inventory. No silent binary/encoding/link skips.

Reports contain locations/rule IDs/hashes, not matched personal text. Identity
profiles, exceptions and reports must remain private. Email is explicitly allowed.
This detects known patterns, not every possible disclosure; review is still needed.
"""
from __future__ import annotations

import base64
import hashlib
import ipaddress
import json
import re
import unicodedata
from pathlib import Path
from urllib.parse import unquote

from .media import clean_raster

MAX_FILE_BYTES = 4 * 1024 * 1024
MAX_DECODE_BYTES = 512 * 1024
EMAIL = re.compile(r"[\w.!#$%&'*+/=?^`{|}~-]+@[\w.-]+\.[A-Za-z]{2,}")
SECRET = re.compile(
    r"-----BEGIN (?:[A-Z ]*PRIVATE KEY|OPENSSH PRIVATE KEY)-----|"
    r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|"
    r"AKIA[A-Z0-9]{16}|sk-(?:ant-)?[A-Za-z0-9_-]{20,})\b|"
    r"(?i:\b(?:authorization\s*[:=]\s*[\"']?bearer\s+)[A-Za-z0-9._~+/-]{12,})|"
    r"(?i:\b(?:api[_-]?key|access[_-]?token|password|secret[_-]?key)\s*[:=]\s*[\"'][^\"'\r\n]{12,}[\"'])"
)
HOME = re.compile(r"(?i)(?:[a-z]:[\\/]+Users[\\/]+|/(?:Users|home)/)([^\s\\/\"'<>]+)")
UNC = re.compile(r"(?i)(?<![:\w\\])\\\\(?![.?]\\)([a-z0-9_.-]+)\\[a-z0-9_$.-]+")
MAC = re.compile(r"(?i)(?<![0-9a-f])(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}(?![0-9a-f])")
SID = re.compile(r"\bS-1-5-21-\d+-\d+-\d+(?:-\d+)?\b")
IP = re.compile(r"(?<![\w.])(?:\d{1,3}\.){3}\d{1,3}(?![\w.])")
PHONE = re.compile(r"(?<![\w.-])(?:\+\d{1,3}[ .-]?)?(?:\(\d{2,4}\)[ .-]?)?\d(?:[ .-]?\d){6,14}(?![\w.-])")
B64 = re.compile(r"(?<![A-Za-z0-9+/])[A-Za-z0-9+/]{12,}={0,2}(?![A-Za-z0-9+/=])")
HEX = re.compile(r"(?<![0-9a-fA-F])[0-9a-fA-F]{16,}(?![0-9a-fA-F])")
NEUTRAL_USERS = {"userprofile", "username", "user", "test", "tester", "me", "example", "example-user", "fixture-user", "alice", "bob", "public", "default", "you", "private", "secret", "café", "o"}


def sha(data: bytes | str) -> str:
    return hashlib.sha256(data.encode("utf-8") if isinstance(data, str) else data).hexdigest()


def canonical(value: str) -> str:
    return unicodedata.normalize("NFKC", value).casefold()


def load_profile(path: Path, forbidden_roots: tuple[Path, ...] = ()) -> dict:
    path = path.resolve(strict=True)
    if any(path.is_relative_to(root.resolve()) for root in forbidden_roots):
        raise ValueError("Identity profile must be outside source/export trees")
    profile = json.loads(path.read_text(encoding="utf-8"))
    if set(profile) != {"identifiers", "phones", "email_allowed"} or profile["email_allowed"] is not True:
        raise ValueError("Profile requires identifiers, phones, email_allowed=true")
    for key in ("identifiers", "phones"):
        if not isinstance(profile[key], list) or any(not isinstance(v, str) or len(v.strip()) < 3 for v in profile[key]):
            raise ValueError("Invalid private identity profile")
    if not profile["identifiers"]:
        raise ValueError("Empty identity profile")
    return profile


def decode_text(data: bytes) -> str:
    if data.startswith((b"\xff\xfe", b"\xfe\xff")):
        return data.decode("utf-16")
    text = data.decode("utf-8-sig")
    if "\0" in text or any(ord(c) < 9 or 13 < ord(c) < 32 for c in text):
        raise ValueError("Non-text control bytes")
    return text


def views(text: str):
    """Bounded multi-pass decoding; one issue makes this an unverified surface."""
    queue = [("plain", text, 0)]
    seen = set()
    total = 0
    while queue:
        layer, value, depth = queue.pop(0)
        if value in seen:
            continue
        seen.add(value)
        total += len(value.encode("utf-8"))
        if total > MAX_FILE_BYTES * 4:
            raise ValueError("Decode expansion limit")
        yield layer, value
        if depth >= 3:
            continue
        candidates = []
        if "%" in value:
            candidates.append(("url", unquote(value)))
        if "\\" in value:
            # JSON escaping, escaped ASCII/Unicode, and doubled Windows slashes.
            def unicode_escape(match):
                return chr(int(match[0][2:], 16))
            candidates.append(("escape", re.sub(r"\\u[0-9a-fA-F]{4}|\\x[0-9a-fA-F]{2}", unicode_escape, value).replace("\\\\", "\\")))
        for kind, pattern in (("base64", B64), ("hex", HEX)):
            for match in pattern.finditer(value):
                token = match[0]
                if len(token) > MAX_DECODE_BYTES * 2:
                    raise ValueError("Encoded token too large to inspect")
                try:
                    data = bytes.fromhex(token) if kind == "hex" else base64.b64decode(token + "=" * (-len(token) % 4), validate=True)
                except ValueError:
                    continue
                for encoding in ("utf-8", "utf-16-le", "utf-16-be"):
                    try:
                        decoded = data.decode(encoding)
                        if decoded and all(c.isprintable() or c in "\r\n\t" for c in decoded):
                            candidates.append((kind, decoded))
                    except UnicodeError:
                        pass
        for kind, decoded in candidates:
            if decoded != value:
                queue.append((layer + "/" + kind, decoded, depth + 1))


def rules(text: str, profile: dict) -> set[str]:
    hits = set()
    if SECRET.search(text):
        hits.add("secret")
    # Explicit user exception: emails may remain, even when their local part is
    # identifying. This does NOT promise unlinkability. Secrets are checked first.
    visible = EMAIL.sub("<EMAIL_ALLOWED>", text)
    norm = canonical(visible)
    compact = re.sub(r"[\W_]", "", norm)
    for item in profile["identifiers"]:
        term = canonical(item)
        if term in norm or re.sub(r"[\W_]", "", term) in compact:
            hits.add("identity")
    digits = re.sub(r"\D", "", visible)
    if any(re.sub(r"\D", "", p) in digits for p in profile["phones"]):
        hits.add("phone_exact")
    for match in HOME.finditer(visible):
        username = canonical(match[1]).rstrip(").,;]}")
        if username not in NEUTRAL_USERS and not re.fullmatch(r"userprofile\d*", username):
            hits.add("home_path")
    if any(canonical(m[1]) not in {"server", "host", "example-server", "localhost", "global", "local"} for m in UNC.finditer(visible)):
        hits.add("network_path")
    if MAC.search(visible):
        hits.add("mac_address")
    if SID.search(visible):
        hits.add("windows_sid")
    for match in IP.finditer(visible):
        try:
            ip = ipaddress.ip_address(match[0])
            if ip.is_private and not ip.is_loopback and not ip.is_unspecified:
                hits.add("private_ip")
        except ValueError:
            pass
    if re.search(r"(?i)(?<!\w)(?:fe80|f[cd][0-9a-f]{2}):[0-9a-f:]+", visible):
        hits.add("private_ipv6")
    for match in PHONE.finditer(visible):
        token = match[0]
        n = len(re.sub(r"\D", "", token))
        # Plain numeric constants are ambiguous, not automatically telephone
        # contacts. They go to Jev/context review. Known private phone is exact.
        groups = re.findall(r"\d+", token)
        grouped_phone = 3 <= len(groups) <= 4 and len(groups[-1]) == 4 and all(2 <= len(g) <= 4 for g in groups)
        if 10 <= n <= 15 and (token.startswith("+") or "(" in token or grouped_phone):
            hits.add("phone_pattern")
    return hits


def text_findings(path: str, text: str, profile: dict) -> list[dict]:
    found = []
    for layer, value in views(text):
        line_rules = set()
        for number, line in enumerate(value.splitlines(), 1):
            current = rules(line, profile)
            line_rules.update(current)
            for rule in sorted(current):
                found.append(finding(path, number if layer == "plain" else 0, rule, layer, sha(line)))
        # Split identifiers/paths/secrets across adjacent lines are not invisible.
        joined = re.sub(r"[\r\n]+\s*", "", value)
        additional = rules(joined, profile) - line_rules
        for rule in sorted(additional):
            found.append(finding(path, 0, rule, layer + "/joined", sha(joined)))
    return found


def finding(path: str, line: int, rule: str, layer: str, digest: str) -> dict:
    item = {"path": path, "line": line, "rule": rule, "layer": layer, "digest": digest}
    return {**item, "id": sha(json.dumps(item, sort_keys=True))}


def inspect(root: Path, profile: dict, approvals: dict | None = None) -> dict:
    root = root.resolve(strict=True)
    approvals = approvals or {}
    files, found = [], []
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root).as_posix()
        if relative == ".git" or relative.startswith(".git/"):
            continue  # Git objects/metadata are verified by release.verify_git.
        if path.is_symlink() or (hasattr(path, "is_junction") and path.is_junction()):
            found.append(finding(relative, 0, "symlink", "path", ""))
            continue
        if path.is_dir():
            continue
        data = path.read_bytes()
        digest = sha(data)
        record = {"path": relative, "sha256": digest, "bytes": len(data)}
        files.append(record)
        found.extend(text_findings(relative, relative, profile))
        if len(data) > MAX_FILE_BYTES:
            found.append(finding(relative, 0, "oversize", "bytes", digest))
            continue
        try:
            text = decode_text(data)
        except (UnicodeError, ValueError):
            record["kind"] = "binary"
            approval = approvals.get(relative, {})
            if not (clean_raster(relative, data) and approval.get("sha256") == digest and approval.get("reviewed") is True and
                    approval.get("metadata_stripped") is True and approval.get("ocr_clean") is True):
                found.append(finding(relative, 0, "unreviewed_binary", "bytes", digest))
            continue
        record["kind"] = "text"
        record["lines"] = len(text.splitlines())
        try:
            found.extend(text_findings(relative, text, profile))
        except ValueError:
            found.append(finding(relative, 0, "unverified_encoding", "bytes", digest))
    unique = {item["id"]: item for item in found}
    manifest = sha(json.dumps(files, sort_keys=True))
    return {"schema": 1, "manifest": manifest, "files": files, "findings": list(unique.values()),
            "clean": not unique, "limitations": "Known-pattern checks only; semantic, visual and independent review required."}


def mask_secrets(text: str) -> str:
    """Never serialize raw potential credentials to Jev/Luna or their logs."""
    if "PRIVATE KEY-----" in text or "[REDACTED_PRIVATE_KEY]" in text:
        return "[REDACTED_PRIVATE_KEY]"  # Caller masks whole private-key blocks first.
    return SECRET.sub("[REDACTED_SECRET]", text)


def mask_document(text: str) -> str:
    text = re.sub(r"-----BEGIN [^-]*PRIVATE KEY-----.*?-----END [^-]*PRIVATE KEY-----", "[REDACTED_PRIVATE_KEY]", text, flags=re.S)
    # Mask common assignment values, including multiline and unquoted secrets.
    text = re.sub(r"(?im)^(\s*[\"']?(?:api[_-]?key|access[_-]?token|password|secret[_-]?key)[\"']?\s*[:=]).*$", r"\1 [REDACTED_SECRET]", text)
    return "\n".join(mask_secrets(line) for line in text.split("\n"))
