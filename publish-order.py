#!/usr/bin/env python3
"""Work out the order the crates must be published in, and prove it.

crates.io refuses a crate whose dependency it has not seen at the version being
published, so the order is not cosmetic: get it wrong and the publish stops
part-way with half the workspace released.

**Computed, never kept by hand.** A hand-written list was what made one release
retry nineteen times: `rustlavel-auth` had gained an *optional* dependency on
`rustlavel-cache`, the list still had auth first, and nothing said so until
crates.io refused it. So this reads every manifest, counts every kind of
dependency — normal, optional, dev and build alike — and asserts the result
before printing it.

Prints the order, one crate per line, so a release script can read it:

    for crate in $(python3 publish-order.py); do
        (cd framework && cargo publish -p "$crate")
    done

Deliberately not written to a file. A generated list checked into the tree is a
list somebody reads after it has gone stale, which is the failure this script
exists to prevent.
"""

import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).parent / "framework"


def main() -> int:
    manifests = sorted((ROOT / "crates").glob("*/Cargo.toml"))
    if not manifests:
        print(f"no crates under {ROOT / 'crates'}", file=sys.stderr)
        return 1

    texts = {}
    for manifest in manifests:
        text = manifest.read_text()
        name = re.search(r'^name\s*=\s*"([^"]+)"', text, re.M)
        if name:
            texts[name.group(1)] = text
    names = set(texts)

    # `[a-z0-9-]`, not `[a-z-]`: `rustlavel-i18n` has digits in it, and a class
    # that excluded them dropped the edge putting i18n before the meta-crate —
    # silently, because a missing edge still produces a plausible-looking order.
    pattern = r"rustlavel-[a-z0-9-]+"
    crates = {}
    for name, text in texts.items():
        deps = set(re.findall(rf"^({pattern})\s*[.=]", text, re.M))
        deps |= set(re.findall(rf'"({pattern})"', text))
        crates[name] = {dep for dep in deps if dep in names and dep != name}

    order: list[str] = []
    seen: set[str] = set()
    visiting: set[str] = set()

    def visit(name: str) -> None:
        if name in seen:
            return
        if name in visiting:
            raise SystemExit(f"dependency cycle reaching {name}")
        visiting.add(name)
        for dep in sorted(crates.get(name, ())):
            visit(dep)
        visiting.discard(name)
        seen.add(name)
        order.append(name)

    for name in sorted(crates):
        visit(name)

    # **Every mention is accounted for.** This check matters more than the one
    # below it, and it was added after the one below it was found to be
    # useless on its own: an ordering assertion can only check edges it knows
    # about, so a pattern that drops a crate name produces a *plausible* order
    # and passes. Narrowing the pattern to `[a-z-]` — losing `rustlavel-i18n`
    # to the digits in its name — is caught here and was not caught there.
    for name, text in texts.items():
        for mentioned in set(re.findall(r"rustlavel-[a-z0-9_-]+", text)):
            mentioned = mentioned.rstrip("-_")
            if mentioned in names and mentioned != name and mentioned not in crates[name]:
                raise SystemExit(
                    f"{name}'s manifest mentions {mentioned} and the dependency "
                    f"pattern did not pick it up — the order below would be missing an edge"
                )

    # And the order itself follows from the edges.
    at = {name: index for index, name in enumerate(order)}
    for name, deps in crates.items():
        for dep in deps:
            if at[dep] >= at[name]:
                raise SystemExit(f"{dep} would be published after {name}, which needs it")

    print("\n".join(order))
    print(f"{len(order)} crates, every dependency before its dependents", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
