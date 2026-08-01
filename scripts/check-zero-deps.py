#!/usr/bin/env python3
"""Enforce D-001: zero algorithm dependencies.

Every workspace crate except those in ALLOWED may depend only on other
workspace crates -- no external crates, including dev-dependencies (the plan
calls for hand-rolled test harnesses rather than proptest/criterion).

Adding an external dependency requires editing ALLOWED on purpose, in a diff
someone reviews. That is the whole point: the rule is enforced, not trusted.
"""

import json
import subprocess
import sys

# hane-wasm holds the browser FFI glue (wasm-bindgen, web-sys). That is
# generated binding code for WebGL/canvas/events, not algorithms -- D-001.
ALLOWED = {"hane-wasm"}


def main() -> int:
    meta = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--no-deps"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )

    members = {pkg["name"] for pkg in meta["packages"]}
    violations = []

    for pkg in meta["packages"]:
        if pkg["name"] in ALLOWED:
            continue
        for dep in pkg["dependencies"]:
            if dep["name"] not in members:
                violations.append((pkg["name"], dep["name"], dep.get("kind")))

    if violations:
        for crate, dep, kind in violations:
            suffix = f" [{kind}-dependency]" if kind else ""
            print(
                f"D-001 violation: {crate} depends on external crate {dep}{suffix}",
                file=sys.stderr,
            )
        print(
            f"\n{len(violations)} violation(s). If this is deliberate, add the crate "
            f"to ALLOWED in {__file__} and say why in the commit message.",
            file=sys.stderr,
        )
        return 1

    checked = len(members - ALLOWED)
    print(f"D-001 ok: {checked} crates, zero external dependencies")
    return 0


if __name__ == "__main__":
    sys.exit(main())
