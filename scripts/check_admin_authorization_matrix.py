#!/usr/bin/env python3
"""Check that every admin entrypoint in admin.rs appears in docs/admin_authorization_matrix.md.

Parses `subscription_vault/src/admin.rs` for public functions inside a#[contractimpl]` blocks and verifies that each one is mentioned in the authorization matrix document.

Exits non-zero with a diagnostic if any entrypoint is missing from the matrix.
"""

from __future__ import annotations

import argparse
import re
import sys
import pathlib
from collections.abc import Iterable
from typing import List, Tuple


REPOSITORY_ROOT = pathlib.Path(__file__).resolve().parent.parent

DEFAULT_ADMIN_RS = REPOSITORY_ROOT / "contracts" / "subscription_vault" / "src" / "admin.rs"
DEFAULT_MATRIX_MD = REPOSITORY_ROOT / "docs" / "admin_authorization_matrix.md"

# Matches an outer `#[contractimpl]` attribute on its own line (optionally with arguments).
CONTRACTIMPL_RE = re.compile(r"^\s*#\[contractimpl(?:\([^\]]*\))?\]")

# Matches a `pub fn name` declaration. Matches the first identifier after `fn`.
PUB_FN_RE = re.compile(r"^\s*pub\s+fn\s+([A-Za-z_Z9][A-Za-z_0-9]*)\s*(?:<[^>]*>)?\s*\(")

# Matches an `fn name` declaration (non-pub).
FN_RE = re.compile(r"^\s*fn\s+([A-Za-z_Z9][A-Za-z_0-9]*)\s*(?:<[^>]*>)?\s*\(")

# Matches an `impl` block opening like `impl SomeType {`.
IMPL_RE = re.compile(r"^\s*impl\s+[^{]*\{")

# Matches an `impl` block opening with a specific trait, like `impl Trait for Type {`.
IMPL_FOR_RE = re.compile(r"^\s*impl\s+[^{]*\bfor\b[^{]*\{")


# Entrypoints that are intentionally not part of the admin matrix.
# These are generally public read-only or non-admin entrypoints that live in
# admin.rs for historical reasons but are documented elsewhere.
EXCLUDED_ENTRYPOINTS = frozenset(
    {
        "init",
        "get_admin",
        "get_operator",
        "get_operator_nonce",
        "get_admin_nonce",
    }
)


def _strip_comments(content: str) -> str:
    """Remove `//` line comments and `/* */` block comments from Rust source."""
    out: List[str] = []
    i = 0
    n = len(content)
    while i < n:
        ch = content[i]
        if ch == "/" and i + 1 < n and content[i + 1] == "/":
            # Line comment: skip to end of line, but keep the newline.
            j = content.find("\n", i)
            if j == -1:
                break
            out.append("\n")
            i = j + 1
        elif ch == "/" and i + 1 < n and content[i + 1] == "*":
            # Block comment: skip to the closing */.
            j = content.find("*/", i + 2)
            if j == -1:
                break
            # Preserve newlines so line numbers stay accurate.
            out.append("\n" * content[i : j + 2].count("\n"))
            i = j + 2
        else:
            out.append(ch)
            i += 1
    return "".join(out)


def _find_matching_brace(content: str, open_idx: int) -> int:
    """Return the index of the ` }` that closes the ``{` at `open_idx`."""
    assert content[open_idx] == "{"
    depth = 0
    i = open_idx
    n = len(content)
    while i < n:
        ch = content[i]
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return -1


def _extract_contractimpl_blocks(content: str) -> Iterable[Tuple[int, int]]:
    """Yield (start, end) offsets for each `#[contractimpl]` impl block."""
    lines = content.splitlines(keepends=True)
    offsets = []
    pos = 0
    for line in lines:
        offsets.append(pos)
        pos += len(line)

    for idx, line in enumerate(lines):
        if not CONTRACTIMPL_RE.match(line):
            continue
        # Find the next `impl `... {` block that follows.
        j = idx + 1
        while j < len(lines):
            candidate = lines[j]
            if IMPL_RE.match(candidate) or IMPL_FOR_RE.match(candidate):
                break
            j += 1
        if j >= len(lines):
            continue
        open_idx = content.find("{", offsets[j])
        if open_idx < 0:
            continue
        close_idx = _find_matching_brace(content, open_idx)
        if close_idx < 0:
            continue
        yield (open_idx + 1, close_idx)


def extract_entrypoints(content: str) -> List[str]:
    """Return the public fn names declared inside `#[contractimpl]` blocks."""
    cleaned = _strip_comments(content)
    names: List[str] = []
    seen = set()
    for start, end in _extract_contractimpl_blocks(cleaned):
        body = cleaned[start:end]
        for line in body.splitlines():
            m = PUB_FN_RE.match(line)
            if not m:
                continue
            name = m.group(1)
            if name not in seen:
                seen.add(name)
                names.append(name)
    return names


def extract_documented_entrypoints(markup: str) -> Set[str]:
    """Return all `backticked` identifiers that look like function names."""
    candidates: Set[str] = set()
    for m in re.finditer(
        r"`([A-Za-z_Z9][A-Za-z_0-9]*)`$|`([A-Za-z_Z9][A-Za-z_0-9]*)\([^`]*\)`", markup
    ):
        name = m.group(1) or m.group(2)
        if name:
            candidates.add(name)
    return candidates


def check_matrix(
    admin_rs: pathlib.Path,
    matrix_md: pathlib.Path,
) -> Tuple[List[str], List[str]]:
    """Return (missing, extra) entrypoint names."""
    if not admin_rs.is_file():
        raise FileNotFoundError(f"admin.rs not found at {admin_rs}")
    if not matrix_md.is_file():
        raise FileNotFoundError(f"matrix document not found at {matrix_md}")

    source = admin_rs.read_text(encoding="utf-8")
    markup = matrix_md.read_text(encoding="utf-8")

    entrypoints = extract_entrypoints(content=source)
    documented = extract_documented_entrypoints(markup)

    missing = [
        name
        for name in entrypoints
        if name not in EXCLUDED_ENTRYPOINTS and name not in documented
    ]
    extra = [
        name
        for name in sorted(documented)
        if name not in entrypoints and name not in EXCLUDED_ENTRYPOINTS and __looks_like_entrypoint(name)
    ]
    return missing, extra


def __looks_like_entrypoint(name: str) -> bool:
    """Helpful heuristic to avoid flagging non-entrypoint backticked tokens."""
    if name in {
        "Role",
        "Admin",
        "Operator",
        "Unauthorized",
        "Forbidden",
        "InvalidInput",
        "Replay",
        "BatchTooLarge",
        "CouponAlreadyExists",
        "CouponAlreadyApplied",
        "CouponTokenMismatch",
        "DataKey::Admin",
        "DataKey::Operator",
        "DOMAIN_OPERATOR_BATCH_CHARGE",
    }:
        return False
    if name.startswith("DataKey::"):
        return False
    if name.isupper():
        return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--admin-rs",
        type=pathlib.Path,
        default=DEFAULT_ADMIN_RS,
        help="Path to subscription_vault/src/admin.rs",
    )
    parser.add_argument(
        "--matrix-md",
        type=pathlib.Path,
        default=DEFAULT_MATRIX_MD,
        help="Path to docs/admin_authorization_matrix.md",
    )
    args = parser.parse_args()

    try:
        missing, extra = check_matrix(admin_rs=args.admin_rs, matrix_md=args.matrix_md)
    except FileNotFoundError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    if missing:
        print(
            "error: the following admin entrypoints are missing from "
            f"{args.matrix_md}:",
            file=sys.stderr,
        )
        for name in missing:
            print(f"  - {name}", file=sys.stderr)
        print(
            "\nAdd a row to the admin authorization matrix for each missing "
            "entrypoint.",
            file=sys.stderr,
        )
        return 1

    if extra:
        print(
            "warning: the following documented entrypoints were not found in "
            f"{args.admin_rs}:",
            file=sys.stderr,
        )
        for name in extra:
            print(f"  - {name}", file=sys.stderr)

    print(
        f"ok: {args.matrix_md} is in sync with {args.admin_rs}",
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
