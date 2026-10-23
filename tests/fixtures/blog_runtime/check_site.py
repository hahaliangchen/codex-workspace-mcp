"""Verify the generated blog under site/ against posts/ and SPEC.md. Prints JSON."""
import html
import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def slug(text):
    # SPEC.md: lowercase, keep letters and digits of any script, other runs -> "-".
    return re.sub(r"[\W_]+", "-", text.lower()).strip("-")


def posts():
    found = []
    for path in sorted((ROOT / "posts").glob("*.md")):
        text = path.read_text(encoding="utf-8")
        _, head, body = text.split("---\n", 2)
        meta = dict(line.split(": ", 1) for line in head.strip().splitlines())
        found.append({
            "file": path.name,
            "title": meta["title"],
            "date": meta["date"],
            "tags": [t.strip() for t in meta["tags"].split(",")],
            "body": [b.strip() for b in body.strip().split("\n\n") if b.strip()],
        })
    return sorted(found, key=lambda p: p["date"], reverse=True)


def read(path, errors):
    try:
        return path.read_text(encoding="utf-8")
    except OSError as error:
        errors.append(f"missing {path.relative_to(ROOT)}: {error.strerror}")
        return None


def text_of(page):
    return html.unescape(re.sub(r"<[^>]+>", " ", page))


def main():
    errors = []
    items = posts()
    site = ROOT / "site"
    slugs = [slug(p["title"]) for p in items]
    for post, name in zip(items, slugs):
        page = read(site / "posts" / f"{name}.html", errors)
        if page is None:
            continue
        where = f"posts/{name}.html"
        if 'data-template="post"' not in page:
            errors.append(f"{where}: not rendered with templates/post.html")
        if f"<h1>{html.escape(post['title'], quote=False)}</h1>" not in page and f"<h1>{post['title']}</h1>" not in page:
            errors.append(f"{where}: missing <h1> title {post['title']!r}")
        if post["date"] not in page:
            errors.append(f"{where}: missing date")
        plain = " ".join(text_of(page).split())
        for block in post["body"]:
            for line in block.splitlines():
                line = line[3:] if line.startswith("## ") else line
                if " ".join(line.split()) not in plain:
                    errors.append(f"{where}: body text missing: {line[:40]!r}")
            if block.startswith("## ") and "<h2>" not in page:
                errors.append(f"{where}: subheading not rendered as <h2>")
        for tag in post["tags"]:
            if f"tags/{slug(tag)}.html" not in page:
                errors.append(f"{where}: missing link to tag {tag!r}")
    index = read(site / "index.html", errors)
    if index is not None:
        if 'data-template="index"' not in index:
            errors.append("index.html: not rendered with templates/index.html")
        positions = []
        for post, name in zip(items, slugs):
            link = f"posts/{name}.html"
            if link not in index:
                errors.append(f"index.html: missing link {link}")
            else:
                positions.append(index.index(link))
            if post["date"] not in index:
                errors.append(f"index.html: missing date of {post['title']!r}")
        if positions != sorted(positions):
            errors.append("index.html: posts are not newest first")
    tags = {}
    for post, name in zip(items, slugs):
        for tag in post["tags"]:
            tags.setdefault(tag, []).append(name)
    for tag, names in tags.items():
        page = read(site / "tags" / f"{slug(tag)}.html", errors)
        if page is None:
            continue
        if tag not in text_of(page):
            errors.append(f"tags/{slug(tag)}.html: missing tag name {tag!r}")
        positions = []
        for name in names:
            if f"{name}.html" not in page:
                errors.append(f"tags/{slug(tag)}.html: missing post {name}")
            else:
                positions.append(page.index(f"{name}.html"))
        if positions != sorted(positions):
            errors.append(f"tags/{slug(tag)}.html: posts are not newest first")
    feed = read(site / "feed.xml", errors)
    if feed is not None:
        links = [f"posts/{name}.html" for name in slugs]
        found = [feed.find(link) for link in links]
        if -1 in found or feed.count("<item>") != len(items):
            errors.append("feed.xml: needs one <item> per post with its posts/<slug>.html link")
        elif found != sorted(found):
            errors.append("feed.xml: items are not newest first")
        if "<pubDate>" not in feed:
            errors.append("feed.xml: missing <pubDate>")
    search = read(site / "search.json", errors)
    if search is not None:
        try:
            entries = json.loads(search)
            if [e.get("url") for e in entries] != [f"posts/{name}.html" for name in slugs]:
                errors.append("search.json: urls missing or not newest first")
            for entry, post in zip(entries, items):
                if entry.get("title") != post["title"] or sorted(entry.get("tags", [])) != sorted(post["tags"]):
                    errors.append(f"search.json: wrong title or tags for {post['title']!r}")
                if post["body"][0].splitlines()[0] not in entry.get("text", ""):
                    errors.append(f"search.json: text missing for {post['title']!r}")
        except ValueError as error:
            errors.append(f"search.json: invalid JSON: {error}")
    # The shared helper itself must follow SPEC.md; build.py must use it.
    sys.path.insert(0, str(ROOT))
    try:
        from lib.slug import slugify
        for title in [p["title"] for p in items] + [t for p in items for t in p["tags"]]:
            if slugify(title) != slug(title):
                errors.append(f"lib/slug.py: slugify({title!r}) is {slugify(title)!r}, SPEC.md requires {slug(title)!r}")
    except Exception as error:  # noqa: BLE001
        errors.append(f"lib/slug.py: {error}")
    build = read(ROOT / "build.py", errors) or ""
    if "slugify" not in build:
        errors.append("build.py: must use lib.slug.slugify")
    if "markdown_lite" not in build:
        errors.append("build.py: must render bodies with lib.markdown_lite")
    tests = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-t", "."], cwd=ROOT, capture_output=True, text=True)
    if tests.returncode != 0:
        errors.append("unit tests failed: " + tests.stderr.strip().splitlines()[-1] if tests.stderr.strip() else "unit tests failed")
    if not (ROOT / "tests" / "test_build.py").is_file():
        errors.append("tests/test_build.py: missing")
    print(json.dumps({"passed": not errors, "posts": len(items), "tags": len(tags), "errors": errors}, ensure_ascii=False, indent=1))
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(main())
