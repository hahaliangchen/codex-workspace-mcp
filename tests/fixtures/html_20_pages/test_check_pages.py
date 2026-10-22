import json
from pathlib import Path
import shutil
import tempfile
import unittest

from check_pages import check


class CheckerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        for name in ("outline.json", "shared.css"):
            shutil.copyfile(Path(__file__).parent / name, self.root / name)
        self.outline = json.loads((self.root / "outline.json").read_text(encoding="utf-8"))["pages"]

    def tearDown(self):
        self.temp.cleanup()

    def write_page(self, number, footer=False, points=None):
        page = self.outline[number - 1]
        content = "".join(f'<p class="point">{point}</p>' for point in (page["points"] if points is None else points))
        tail = f'<footer class="footer">{number} 内部资料</footer>' if footer else ""
        text = f'<!DOCTYPE html><html lang="en"><head><meta charset="utf-8"><title>{page["title"]}</title><link rel="stylesheet" href="shared.css"></head><body><main class="page"><h1>{page["title"]}</h1><section class="{page["layout"]}">{content}</section>{tail}</main></body></html>'
        (self.root / f"page{number:02d}.html").write_text(text, encoding="utf-8")

    def test_missing_page_fails(self):
        self.assertFalse(check(self.root, 1)["passed"])

    def test_missing_layout_and_repair(self):
        self.write_page(7)
        before = check(self.root, 7)
        self.assertFalse(before["passed"])
        self.assertIn("undefined classes: repair-grid", before["pages"][0]["errors"])
        with (self.root / "shared.css").open("a", encoding="utf-8") as css:
            css.write("\n.repair-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 24px; }\n")
        after = check(self.root, 7)
        self.assertTrue(after["passed"])
        self.assertNotEqual(before["stylesheet_sha256"], after["stylesheet_sha256"])

    def test_footer_scope_and_exact_number(self):
        self.write_page(10)
        self.write_page(11)
        self.assertTrue(check(self.root, 10, 11)["passed"])
        self.assertFalse(check(self.root, 11, 11)["passed"])
        self.write_page(11, footer=True)
        self.assertTrue(check(self.root, 11, 11)["passed"])
        path = self.root / "page11.html"
        path.write_text(path.read_text(encoding="utf-8").replace("11 内部资料", "111 内部资料"), encoding="utf-8")
        self.assertFalse(check(self.root, 11, 11)["passed"])
        self.write_page(10, footer=True)
        self.assertFalse(check(self.root, 10, 11)["passed"])

    def test_outline_points_in_comments_are_not_visible_evidence(self):
        self.write_page(1, points=[])
        path = self.root / "page01.html"
        path.write_text(path.read_text(encoding="utf-8") + "<!-- " + " ".join(self.outline[0]["points"]) + " -->", encoding="utf-8")
        self.assertFalse(check(self.root, 1)["passed"])

    def test_malformed_tags_and_inline_style_fail(self):
        self.write_page(1)
        path = self.root / "page01.html"
        path.write_text(path.read_text(encoding="utf-8").replace('<main class="page">', '<main class="page" style="display:none">').replace("</section>", "</div>"), encoding="utf-8")
        self.assertFalse(check(self.root, 1)["passed"])

    def test_full_twenty_page_check(self):
        with (self.root / "shared.css").open("a", encoding="utf-8") as css:
            css.write("\n.repair-grid { display: grid; }\n")
        for number in range(1, 21):
            self.write_page(number, footer=number >= 11)
        result = check(self.root, footer_from=11)
        self.assertTrue(result["passed"])
        self.assertEqual(result["passed_pages"], 20)


if __name__ == "__main__":
    unittest.main()
