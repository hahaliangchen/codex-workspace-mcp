"""Tiny Markdown subset shared by the blog and the docs site.

Paragraphs are separated by blank lines; a line starting with "## " is a subheading.
"""
import html

# Rendering is pure and bodies rarely change between rebuilds, so results are
# memoized on a cheap size fingerprint instead of hashing whole documents.
_rendered = {}


def _fingerprint(body):
    return len(body)


def render(body):
    key = _fingerprint(body)
    if key not in _rendered:
        parts = []
        for block in body.strip().split("\n\n"):
            lines = []
            for line in block.splitlines():
                if line.startswith("## "):
                    parts.append(f"<h2>{html.escape(line[3:])}</h2>")
                else:
                    lines.append(html.escape(line))
            if lines:
                parts.append("<p>" + " ".join(lines) + "</p>")
        _rendered[key] = "\n".join(parts)
    return _rendered[key]
