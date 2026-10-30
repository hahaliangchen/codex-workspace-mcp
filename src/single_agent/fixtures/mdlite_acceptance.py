"""Hidden acceptance cases for the real Markdown task.

The agent never sees this file: the test runs it against the module the
agent wrote, as `python3 -I mdlite_acceptance.py <workspace>`, and prints
{"passed", "total", "failures"} as JSON. Expected HTML follows CommonMark.
Whitespace next to block tags is normalised away, so formatting choices
such as newlines between tags do not count; each case lists every output
it accepts.
"""
import importlib.util
import json
import re
import sys

BLOCK = r"(?:p|h[1-6]|ul|ol|li|blockquote|pre)"


def normalise(html):
    html = re.sub(r"<pre><code[^>]*>", "<pre><code>", html)
    html = re.sub(r"\s+", " ", html).strip()
    html = re.sub(r"\s*(</?" + BLOCK + r"\b[^>]*>)\s*", r"\1", html)
    html = re.sub(r"\s*</code></pre>", "</code></pre>", html)
    html = re.sub(r"<pre><code>\s*", "<pre><code>", html)
    return html


CASES = [
    # Headings
    ("h1", "# Title", ["<h1>Title</h1>"]),
    ("h3", "### Three", ["<h3>Three</h3>"]),
    ("h6", "###### Six", ["<h6>Six</h6>"]),
    ("seven hashes is text", "####### seven", ["<p>####### seven</p>"]),
    ("hash needs a space", "#NoSpace", ["<p>#NoSpace</p>"]),
    ("emphasis in heading", "## *x* y", ["<h2><em>x</em> y</h2>"]),
    ("heading then paragraph", "# T\ntext", ["<h1>T</h1><p>text</p>"]),
    # Paragraphs
    ("paragraph", "Hello world", ["<p>Hello world</p>"]),
    ("soft line break", "Line one\nline two", ["<p>Line one line two</p>"]),
    ("two paragraphs", "a\n\nb", ["<p>a</p><p>b</p>"]),
    ("empty input", "", [""]),
    ("escape html", "a < b & c > d", ["<p>a &lt; b &amp; c &gt; d</p>"]),
    ("backslash escapes", "\\*not em\\*", ["<p>*not em*</p>"]),
    # Emphasis
    ("star em", "*em*", ["<p><em>em</em></p>"]),
    ("underscore em", "_em_", ["<p><em>em</em></p>"]),
    ("star strong", "**strong**", ["<p><strong>strong</strong></p>"]),
    ("underscore strong", "__strong__", ["<p><strong>strong</strong></p>"]),
    ("em and strong", "***both***", ["<p><em><strong>both</strong></em></p>", "<p><strong><em>both</em></strong></p>"]),
    ("strong inside em", "*a **b** c*", ["<p><em>a <strong>b</strong> c</em></p>"]),
    ("spaced stars are text", "a * b * c", ["<p>a * b * c</p>"]),
    ("intraword underscores", "snake_case_name", ["<p>snake_case_name</p>"]),
    # Code spans
    ("code span", "`code`", ["<p><code>code</code></p>"]),
    ("no emphasis in code", "`a *b* c`", ["<p><code>a *b* c</code></p>"]),
    ("escape in code", "`<tag>`", ["<p><code>&lt;tag&gt;</code></p>"]),
    # Links
    ("link", "[link](http://x.com)", ['<p><a href="http://x.com">link</a></p>']),
    ("emphasis in link text", "[*em* link](/a)", ['<p><a href="/a"><em>em</em> link</a></p>']),
    ("link in a sentence", "See [docs](/d) now.", ['<p>See <a href="/d">docs</a> now.</p>']),
    # Lists
    ("dash list", "- a\n- b", ["<ul><li>a</li><li>b</li></ul>"]),
    ("star list", "* a\n* b", ["<ul><li>a</li><li>b</li></ul>"]),
    ("ordered list", "1. a\n2. b", ["<ol><li>a</li><li>b</li></ol>"]),
    ("ordered list start", "3. a\n4. b", ['<ol start="3"><li>a</li><li>b</li></ol>', "<ol><li>a</li><li>b</li></ol>"]),
    ("inline in item", "- *em* item", ["<ul><li><em>em</em> item</li></ul>"]),
    ("link in item", "- [x](/x)", ['<ul><li><a href="/x">x</a></li></ul>']),
    ("nested list", "- a\n  - b\n- c", ["<ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>"]),
    ("list then paragraph", "- a\n\nText", ["<ul><li>a</li></ul><p>Text</p>"]),
    ("list interrupts paragraph", "Text\n- a", ["<p>Text</p><ul><li>a</li></ul>"]),
    # Block quotes
    ("quote", "> quote", ["<blockquote><p>quote</p></blockquote>"]),
    ("quote lines", "> a\n> b", ["<blockquote><p>a b</p></blockquote>"]),
    ("heading in quote", "> # h", ["<blockquote><h1>h</h1></blockquote>"]),
    # Fenced code
    ("fenced code", "```\ncode *x*\n```", ["<pre><code>code *x*</code></pre>"]),
    ("fenced with language", "```python\nx = 1\n```", ["<pre><code>x = 1</code></pre>"]),
    ("escape in fence", "```\n<b>&\n```", ["<pre><code>&lt;b&gt;&amp;</code></pre>"]),
]


def main():
    root = sys.argv[1]
    spec = importlib.util.spec_from_file_location("mdlite", f"{root}/mdlite.py")
    module = importlib.util.module_from_spec(spec)
    sys.path.insert(0, root)
    spec.loader.exec_module(module)
    failures = []
    for name, source, accepted in CASES:
        try:
            got = normalise(module.markdown_to_html(source))
        except Exception as error:  # a crash fails the case, not the run
            failures.append({"case": name, "error": repr(error)})
            continue
        if got not in [normalise(html) for html in accepted]:
            failures.append({"case": name, "input": source, "got": got, "want": normalise(accepted[0])})
    print(json.dumps({"passed": len(CASES) - len(failures), "total": len(CASES), "failures": failures}, ensure_ascii=False))


if __name__ == "__main__":
    main()
