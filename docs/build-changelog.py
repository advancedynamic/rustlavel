#!/usr/bin/env python3
"""Render CHANGELOG.md into docs/changelog.html.

One source of truth. A changelog kept twice is a changelog that disagrees with
itself by the second release, and the page nobody edited is the one people read.

The renderer covers exactly what CHANGELOG.md uses — headings, bullets with
indented continuations, fenced blocks, `code`, **bold**, and links — and
**raises on anything else**. A silently dropped construct would be a paragraph
missing from a release note, which is the kind of thing nobody notices until it
matters.

Run it from the repository root:

    python3 docs/build-changelog.py
"""

import html
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "CHANGELOG.md"
TARGET = ROOT / "docs" / "changelog.html"

# `code`, then **bold**, then [text](url). Order matters: code is taken first so
# a `**` inside a code span is left alone.
CODE = re.compile(r"`([^`]+)`")
BOLD = re.compile(r"\*\*([^*]+)\*\*")
LINK = re.compile(r"\[([^\]]+)\]\(([^)]+)\)")


def inline(text):
    """Escape, then put back the three inline forms as markup.

    Code spans are lifted out before the bold pass and put back after. A code
    span can contain an asterisk — `MAIL_*` does — and leaving it in place made
    the bold regex pair the wrong asterisks, which showed up on the page as a
    <strong> opening in the middle of one sentence and closing in the next.
    """
    out = html.escape(text)

    spans = []

    def park(match):
        spans.append(match.group(1))
        return f"\x00{len(spans) - 1}\x00"

    out = CODE.sub(park, out)
    out = BOLD.sub(lambda m: f"<strong>{m.group(1)}</strong>", out)
    out = LINK.sub(lambda m: f'<a href="{m.group(2)}">{m.group(1)}</a>', out)

    for at, span in enumerate(spans):
        out = out.replace(f"\x00{at}\x00", f"<code>{span}</code>")
    return out


def render(source):
    lines = source.split("\n")
    out, at = [], 0
    # The prose above the first version heading is the file's own preamble.
    while at < len(lines) and not lines[at].startswith("## "):
        at += 1

    open_list = False
    open_item = False
    open_nested = False

    def close_nested():
        nonlocal open_nested
        if open_nested:
            out.append("</li></ul>")
            open_nested = False

    def close_item():
        nonlocal open_item
        close_nested()
        if open_item:
            out.append("</li>")
            open_item = False

    def close_list():
        nonlocal open_list
        close_item()
        if open_list:
            out.append("</ul>")
            open_list = False

    while at < len(lines):
        line = lines[at]
        stripped = line.strip()

        if line.startswith("## "):
            close_list()
            # "0.7.2 — 2026-09-05" becomes an id of "0-7-2".
            title = line[3:].strip()
            version = title.split("—")[0].strip().replace(".", "-").lower()
            out.append(f'<h2 id="{html.escape(version)}">{inline(title)}</h2>')
            at += 1
            continue

        if line.startswith("### "):
            close_list()
            out.append(f"<h3>{inline(line[4:].strip())}</h3>")
            at += 1
            continue

        if line.startswith("- "):
            close_item()
            if not open_list:
                out.append("<ul>")
                open_list = True
            block = [line[2:].strip()]
            at += 1
            while at < len(lines) and lines[at].strip() and not lines[at].strip().startswith(("- ", "```")):
                block.append(lines[at].strip())
                at += 1
            out.append(f"<li>{inline(' '.join(block))}")
            open_item = True
            continue

        if stripped.startswith("```"):
            # A fenced block, indented under the bullet it belongs to.
            at += 1
            body = []
            while at < len(lines) and not lines[at].strip().startswith("```"):
                body.append(lines[at][2:] if lines[at].startswith("  ") else lines[at])
                at += 1
            at += 1
            out.append("<pre><code>" + html.escape("\n".join(body)) + "</code></pre>")
            continue

        if not stripped:
            at += 1
            continue

        if line.startswith("  - ") and open_item:
            # A nested bullet. Treated as prose before, which joined several
            # `**bold**` spans onto one line and left the pairs interleaved —
            # visible on the page as stray asterisks and <strong> in the middle
            # of sentences.
            if not open_nested:
                out.append("<ul>")
                open_nested = True
            else:
                out.append("</li>")
            block = [line.strip()[2:]]
            at += 1
            while at < len(lines) and lines[at].strip() and not lines[at].strip().startswith(("- ", "```")):
                block.append(lines[at].strip())
                at += 1
            out.append(f"<li>{inline(' '.join(block))}")
            continue

        if line.startswith("  ") and open_item:
            # A continuation of the bullet above. Gathered to the next blank
            # line, because markdown wraps a paragraph across source lines and
            # rendering each one separately would break a sentence into pieces.
            block = []
            while at < len(lines) and lines[at].strip() and not lines[at].strip().startswith("```"):
                block.append(lines[at].strip())
                at += 1
            text = inline(" ".join(block))

            previous = out[-1] if out else ""
            if previous.endswith(("</p>", "</code></pre>")):
                # A fresh paragraph inside the same item.
                out.append(f"<p>{text}</p>")
            else:
                # More of the sentence the bullet started.
                out[-1] = previous + " " + text
            continue

        if not line.startswith(" ") and not open_list:
            # A release can open with a line of its own before the first
            # section — a sentence saying what the release is for.
            paragraph = [stripped]
            at += 1
            while at < len(lines) and lines[at].strip() and not lines[at].startswith(("#", "- ", "  ")):
                paragraph.append(lines[at].strip())
                at += 1
            out.append(f"<p>{inline(' '.join(paragraph))}</p>")
            continue

        raise SystemExit(
            f"CHANGELOG.md:{at + 1}: this renderer does not know what to do with "
            f"{line!r}. Teach it that shape rather than letting the line vanish "
            f"from the page."
        )

    close_list()
    return "\n".join(out)


PAGE = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Changelog — Rustlavel</title>
<meta name="description" content="What changed in each release of Rustlavel, newest first.">
<link rel="stylesheet" href="style.css">
</head>
<body>

<header class="site"><div class="wrap">
  <strong>Rustlavel</strong>
  <nav>
    <a href="index.html">Home</a>
    <a href="guide.html">Guide</a>
    <a href="cookbook.html">Cookbook</a>
    <a href="packages.html">Packages</a>
    <a href="changelog.html" aria-current="page">Changelog</a>
    <a href="https://docs.rs/rustlavel">API</a>
    <a href="https://github.com/advancedynamic/rustlavel">GitHub</a>
  </nav>
</div></header>

<main class="wrap">

<h1>Changelog</h1>
<p class="tagline">Newest first. Every crate in the workspace shares one version number.</p>

<p>Upgrading an existing project&rsquo;s starter-kit files is
<a href="guide.html#upgrading">a separate step</a> from bumping the dependency &mdash;
<code>rustlavel upgrade</code> merges them rather than overwriting what you wrote.</p>

__BODY__

</main>

<footer class="wrap">
  <p>Generated from <a href="https://github.com/advancedynamic/rustlavel/blob/main/CHANGELOG.md">CHANGELOG.md</a>
  by <code>docs/build-changelog.py</code>. Edit the markdown, not this page.</p>
</footer>

</body>
</html>
"""


def check(page):
    """Refuse to write a page that is visibly wrong.

    Every failure this caught was silent — asterisks left on the page, a
    <strong> opened in one sentence and closed in the next, a list that never
    closed. None of them raise on their own, and all of them are obvious once
    somebody counts.
    """
    if "**" in page:
        raise SystemExit(
            "the rendered page still holds `**`, so a bold span was not paired. Usually a "
            "`**` that opens on one source line and closes on another, or an asterisk inside "
            "a code span."
        )
    for opening, closing in [("<strong>", "</strong>"), ("<li>", "</li>"), ("<ul>", "</ul>")]:
        if page.count(opening) != page.count(closing):
            raise SystemExit(
                f"{page.count(opening)} {opening} against {page.count(closing)} {closing}: "
                "the markup does not balance, and a browser will guess where things end."
            )
    return page


def main():
    if not SOURCE.is_file():
        raise SystemExit(f"{SOURCE} is not there; run this from the repository root.")
    TARGET.write_text(check(PAGE.replace("__BODY__", render(SOURCE.read_text()))))
    print(f"wrote {TARGET.relative_to(ROOT)}")


if __name__ == "__main__":
    sys.exit(main())
