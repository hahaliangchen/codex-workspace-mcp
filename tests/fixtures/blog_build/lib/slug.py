"""Shared slug helper used by the blog build and the docs site."""
import re


def slugify(text):
    text = text.lower()
    text = re.sub(r"[^a-z0-9]+", "-", text)
    return text.strip("-")
