# Static blog build

Write `build.py` (Python 3, standard library only) that turns `posts/*.md` into a static site under `site/`.

## Input

Each post starts with a front-matter block:

```
---
title: Hello, World!
date: 2026-01-05
tags: intro, meta
---
```

The rest is the body: paragraphs separated by blank lines. Lines starting with `## ` are subheadings.

## Output

- `site/posts/<slug>.html` for every post, rendered with `templates/post.html`. Paragraphs become `<p>`, subheadings `<h2>`. Each tag links to its tag page.
- `site/index.html` rendered with `templates/index.html`: every post, newest first, each linking to its post page and showing its date.
- `site/tags/<tag-slug>.html` for every tag: a heading with the tag name and links to its posts, newest first.

- `site/feed.xml`: an RSS 2.0 feed with one `<item>` per post, newest first, each with `<title>`, `<link>` (`posts/<slug>.html`) and `<pubDate>` (RFC 822, e.g. `Mon, 05 Jan 2026 00:00:00 +0000`).
- `site/search.json`: a JSON array, newest first, of `{"title", "url", "tags", "text"}` per post, where `url` is `posts/<slug>.html` and `text` is the plain body text.

## Tests

Add `tests/test_build.py` (unittest) covering front-matter parsing, body rendering and post ordering. `python -m unittest discover -s tests -t .` must pass.

## Slugs

`lib/slug.py` is shared with the docs site. `build.py` must import `slugify` from it and must not implement its own slug rules; the shared helper itself must follow this section. Use it for every post and tag slug. A slug is lowercase; letters and digits of any script (including Chinese) are kept; every other run of characters becomes a single `-`; leading and trailing `-` are removed. Slugs must be unique and never empty.

## Rules

- Do not modify `posts/`, `templates/`, `SPEC.md` or `check_site.py`.
- `python check_site.py` verifies the whole site and prints JSON. The task is done when it reports `"passed": true`.
