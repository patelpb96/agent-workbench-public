"""Auditable policy: judgments belong to Jev; publication decisions stay in code."""
from __future__ import annotations

VERSION = "pii-lines-v1"
MODEL = "jev-1.14.0"  # An alias change must not silently reuse evaluation evidence.
REVIEW_THRESHOLD = 0.15
CATEGORIES = ("name", "phone", "machine", "other_pii")
QUESTIONS = {
    "name": {
        "type": "noul",
        "instructions": "Does the target line identify a private person by name or personal account handle?",
        "criteria": {"true": "A real person's name or personal username occurs outside an email address.",
                     "false": "Only generic roles, public library/product names, placeholders, or email addresses occur."},
    },
    "phone": {
        "type": "noul",
        "instructions": "Does the target line contain a person's telephone number?",
        "criteria": {"true": "A telephone contact number, including an international or formatted number.",
                     "false": "Only dates, versions, counters, hashes, ports, or synthetic identifiers occur."},
    },
    "machine": {
        "type": "noul",
        "instructions": "Does the target line reveal a particular private machine's identity or local configuration?",
        "criteria": {"true": "An actual home directory, personal absolute path, hostname, LAN address, device identifier, or machine-specific account.",
                     "false": "Portable environment variables, relative paths, generic OS APIs, localhost, or clearly synthetic test paths."},
    },
    "other_pii": {
        "type": "noul",
        "instructions": "Does the target line disclose private personal facts beyond names, phone numbers, or machine details?",
        "criteria": {"true": "A home address, private financial/medical fact, government identifier, or private conversation.",
                     "false": "Ordinary code, public documentation, synthetic fixtures, or email addresses only."},
    },
}
# A scrub is a proposal, never execution authority. Even after graduation Jev cannot
# clear a deterministic finding, commit, push, or change a provider's permissions.
LUNA_MODEL = "openai-codex/gpt-6-luna"
LUNA_THINKING = "max"
ANONYMOUS_NAME = "Workbench Contributors"
ANONYMOUS_EMAIL = "anonymous@users.noreply.github.com"
