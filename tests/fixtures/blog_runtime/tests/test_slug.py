import unittest

from lib.slug import slugify


class SlugTest(unittest.TestCase):
    def test_ascii(self):
        self.assertEqual(slugify("Hello, World!"), "hello-world")

    def test_ampersand(self):
        self.assertEqual(slugify("Tags & Taxonomy"), "tags-taxonomy")


if __name__ == "__main__":
    unittest.main()
