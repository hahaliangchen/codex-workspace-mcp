"""Check saved fixture pages using only the Python standard library."""
import argparse
import hashlib
from html.parser import HTMLParser
import json
from pathlib import Path
import re
import sys


VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}
HIDDEN = {"head", "script", "style", "template"}


class Page(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.stack = []
        self.classes = set()
        self.visible = []
        self.headings = []
        self.titles = []
        self.footers = []
        self.errors = []
        self.stylesheet = False
        self.layouts = set()
        self.ids = set()
        self.doctype = False
        self.utf8 = False
        self.main_page = False

    def handle_decl(self, decl):
        self.doctype = decl.lower() == "doctype html"

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        classes = set((attrs.get("class") or "").split())
        self.classes.update(classes)
        if tag == "main" and "page" in classes:
            self.main_page = True
        self.layouts.update(classes)
        if tag in {"style", "script"} or "style" in attrs or any(key.startswith("on") for key in attrs):
            self.errors.append("inline styling or scripting is forbidden")
        if tag == "html" and attrs.get("lang") != "en":
            self.errors.append("html lang must be en")
        if tag == "meta" and (attrs.get("charset") or "").lower() == "utf-8":
            self.utf8 = True
        if "hidden" in attrs:
            self.errors.append("hidden content is forbidden")
        for key in ("src", "href"):
            value = attrs.get(key) or ""
            if re.match(r"^(?:[a-z][a-z0-9+.-]*:|//)", value, re.I):
                self.errors.append("external or embedded resources are forbidden")
        if tag == "link" and attrs.get("rel") == "stylesheet" and attrs.get("href") == "shared.css":
            self.stylesheet = True
        if attrs.get("id"):
            if attrs["id"] in self.ids:
                self.errors.append("duplicate id: " + attrs["id"])
            self.ids.add(attrs["id"])
        if tag == "h1":
            self.headings.append([])
        if tag == "title":
            self.titles.append([])
        if tag == "footer":
            self.footers.append([])
        if tag not in VOID:
            self.stack.append(tag)

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)
        if tag not in VOID:
            self.handle_endtag(tag)

    def handle_endtag(self, tag):
        if tag in VOID or not self.stack or self.stack[-1] != tag:
            self.errors.append("unmatched closing tag: " + tag)
        else:
            self.stack.pop()

    def handle_data(self, data):
        if "title" in self.stack and self.titles:
            self.titles[-1].append(data)
        if not HIDDEN.intersection(self.stack):
            self.visible.append(data)
            if "h1" in self.stack and self.headings:
                self.headings[-1].append(data)
            if "footer" in self.stack and self.footers:
                self.footers[-1].append(data)


def normalized(text):
    return " ".join(text.split())


def check(root, page_number=None, footer_from=None):
    root = Path(root)
    outline = json.loads((root / "outline.json").read_text(encoding="utf-8"))["pages"]
    css = (root / "shared.css").read_bytes()
    # The fixture stylesheet deliberately uses simple literal class selectors.
    rules = re.sub(r"/\*.*?\*/", "", css.decode("utf-8"), flags=re.S)
    selectors = " ".join(re.findall(r"([^{}]+)\{[^{}]*\}", rules))
    defined = set(re.findall(r"\.([a-zA-Z_][\w-]*)", selectors))
    results = []
    for item in outline:
        if page_number is not None and item["id"] != page_number:
            continue
        path = root / f"page{item['id']:02d}.html"
        errors = []
        digest = None
        try:
            raw = path.read_bytes()
            digest = hashlib.sha256(raw).hexdigest()
            if not 300 <= len(raw) <= 120000:
                errors.append("file size must be between 300 and 120000 bytes")
            page = Page()
            page.feed(raw.decode("utf-8"))
            page.close()
            errors.extend(page.errors)
            if page.stack:
                errors.append("unclosed tags: " + ", ".join(page.stack))
            if not page.doctype or not page.utf8:
                errors.append("HTML5 doctype and UTF-8 metadata required")
            if [normalized("".join(text)) for text in page.headings] != [item["title"]]:
                errors.append("exactly one matching h1 required")
            if [normalized("".join(text)) for text in page.titles] != [item["title"]]:
                errors.append("matching title required")
            visible = normalized(" ".join(page.visible))
            positions = [visible.find(point) for point in item["points"]]
            if -1 in positions or positions != sorted(positions):
                errors.append("outline points must appear visibly and in order")
            if not page.stylesheet:
                errors.append("shared.css stylesheet link required")
            if not page.main_page or item["layout"] not in page.layouts:
                errors.append("main.page and required layout class must be present")
            if page.classes - defined:
                errors.append("undefined classes: " + ", ".join(sorted(page.classes - defined)))
            needs_footer = footer_from is not None and item["id"] >= footer_from
            footers = [normalized("".join(text)) for text in page.footers]
            if needs_footer:
                if len(footers) != 1 or "内部资料" not in footers[0] or not re.search(rf"(?<!\d){item['id']}(?!\d)", footers[0]):
                    errors.append("footer must contain the page number and 内部资料")
            elif footers:
                errors.append("footer not permitted on this page")
        except (OSError, UnicodeError, ValueError) as exc:
            errors.append(str(exc))
        results.append({"page": item["id"], "path": path.name, "sha256": digest, "passed": not errors, "errors": errors})
    if page_number is not None and not results:
        raise ValueError("page not found in outline")
    return {"fixture_version": 1, "passed": all(item["passed"] for item in results), "checked": len(results),
            "passed_pages": sum(item["passed"] for item in results), "footer_from": footer_from,
            "stylesheet_sha256": hashlib.sha256(css).hexdigest(), "pages": results}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--page", type=int, choices=range(1, 21))
    parser.add_argument("--footer-from", type=int, choices=[11])
    args = parser.parse_args()
    try:
        result = check(args.root, args.page, args.footer_from)
    except (OSError, ValueError, KeyError) as exc:
        result = {"passed": False, "error": str(exc)}
    print(json.dumps(result, ensure_ascii=True))
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    sys.exit(main())
