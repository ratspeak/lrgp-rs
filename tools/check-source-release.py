#!/usr/bin/env python3
"""Check source-release metadata that must stay true for this library."""

from __future__ import annotations

import json
from pathlib import Path
import re
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[1]
EXPECTED_MSRV = "1.85"
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")


def fail(message: str) -> None:
    print(f"source-release contract: {message}", file=sys.stderr)
    raise SystemExit(1)


metadata = json.loads(
    subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=ROOT,
        check=True,
        capture_output=True,
        text=True,
    ).stdout
)

packages = metadata["packages"]
if len(packages) != 1 or packages[0]["name"] != "lrgp":
    fail("the package inventory changed; update this check deliberately")

package = packages[0]
if package["publish"] != []:
    fail("lrgp must declare publish = false")
if package["rust_version"] != EXPECTED_MSRV:
    fail(
        f"lrgp must declare Rust {EXPECTED_MSRV} "
        f"(found {package['rust_version']!r})"
    )

tracked = subprocess.run(
    ["git", "ls-files", "--error-unmatch", "Cargo.lock"],
    cwd=ROOT,
    capture_output=True,
    text=True,
)
if tracked.returncode == 0:
    fail("Cargo.lock must remain untracked for this library crate")

if "## Unreleased" not in (ROOT / "CHANGELOG.md").read_text(encoding="utf-8"):
    fail("CHANGELOG.md must retain an Unreleased section")

workflows = ROOT / ".github/workflows"
for workflow_path in sorted([*workflows.glob("*.yml"), *workflows.glob("*.yaml")]):
    workflow = workflow_path.read_text(encoding="utf-8")
    action_uses = re.findall(
        r"^\s*(?:-\s+)?uses:\s+([^\s#]+)", workflow, re.MULTILINE
    )
    for action in action_uses:
        if action.startswith("./"):
            continue
        if "@" not in action or not SHA_PATTERN.fullmatch(action.rsplit("@", 1)[1]):
            fail(
                f"workflow action is not pinned to a commit "
                f"({workflow_path.name}): {action}"
            )

print("source-release contract: ok")
