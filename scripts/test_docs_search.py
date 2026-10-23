#!/usr/bin/env python3
"""Tests for extracting searchable documentation from its published HTML."""
import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("docs_search", SCRIPTS / "docs-search.py")
search = importlib.util.module_from_spec(spec)
spec.loader.exec_module(search)


class SearchIndexTests(unittest.TestCase):
    def write(self, web, text):
        (web / "docs.html").write_text(text, encoding="utf-8")

    def test_indexes_rendered_guides_code_and_captions(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            self.write(web, '''<article data-doc-section id="capture" data-doc-title="Capture frames" data-doc-category="Getting started" data-doc-tags="simulator, --destination">
              <h2>Capture <em>frames</em></h2><p class="doc-summary">Save &amp; inspect an image.</p>
              <pre><code>capturefab --camera sim:0 capture -o first.png</code></pre>
              <figure><img src="capture.png" alt="Preview"><figcaption>Crop around the capture controls.</figcaption></figure>
              <button>Copy</button><span aria-hidden="true">Decorative text</span><script>unsearchable()</script>
            </article>''')
            document = search.index(web)["documents"][0]
            self.assertEqual(document["title"], "Capture frames")
            self.assertEqual(document["summary"], "Save & inspect an image.")
            self.assertEqual(document["tags"], ["simulator", "--destination"])
            self.assertIn("--camera sim:0 capture -o first.png", document["body"])
            self.assertIn("Crop around the capture controls.", document["body"])
            for excluded in ("Copy", "Decorative text", "unsearchable"):
                self.assertNotIn(excluded, document["body"])
            self.assertEqual(document["url"], "docs.html#capture")
            self.assertEqual(json.loads(search.generated(web))["schema_version"], 1)

    def test_rejects_missing_summary_and_mismatched_heading(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            self.write(web, '<article data-doc-section id="capture" data-doc-title="Save"><h2>Capture</h2></article>')
            with self.assertRaisesRegex(ValueError, "data-doc-title must match its h2.*missing .doc-summary"):
                search.index(web)

    def test_indexes_all_reference_pages_with_their_own_urls(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            article = '<article data-doc-section id="capture" data-doc-title="Capture"><h2>Capture</h2><p class="doc-summary">Save frames.</p></article>'
            self.write(web, article)
            (web / "reference.html").write_text(article, encoding="utf-8")
            (web / "index.html").write_text('<main>Product page without articles</main>', encoding="utf-8")
            documents = search.index(web)["documents"]
            self.assertEqual([item["url"] for item in documents], ["docs.html#capture", "reference.html#capture"])

    def test_rejects_invalid_sections_on_reference_pages(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            self.write(web, '<main>Task guides</main>')
            (web / "reference.html").write_text('<article data-doc-section id="cli" data-doc-title="CLI"><h2>CLI</h2></article>', encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "reference.html: cli: missing .doc-summary"):
                search.index(web)

    def test_rejects_duplicate_and_unsafe_document_anchors(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            article = '<article data-doc-section id="capture" data-doc-title="Capture"><h2>Capture</h2><p class="doc-summary">Save frames.</p></article>'
            self.write(web, article * 2)
            with self.assertRaisesRegex(ValueError, "duplicate documentation section"):
                search.index(web)
            self.write(web, article.replace('id="capture"', 'id="../../bad"'))
            with self.assertRaisesRegex(ValueError, "invalid documentation section id"):
                search.index(web)

    def test_check_requires_regeneration_after_published_content_changes(self):
        with tempfile.TemporaryDirectory() as temp:
            web = Path(temp)
            article = '<article data-doc-section id="capture" data-doc-title="Capture"><h2>Capture</h2><p class="doc-summary">Save frames.</p></article>'
            self.write(web, article)
            command = [sys.executable, "-B", str(SCRIPTS / "docs-search.py"), "--web", str(web)]
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual(subprocess.run(command + ["--check"], capture_output=True).returncode, 0)
            self.write(web, article.replace("Save frames.", "Save images and sequences."))
            stale = subprocess.run(command + ["--check"], capture_output=True, text=True)
            self.assertEqual(stale.returncode, 1)
            self.assertIn("search-index.json is stale", stale.stderr)
            subprocess.run(command, check=True, capture_output=True)
            self.assertEqual(json.loads((web / "search-index.json").read_text())["documents"][0]["summary"], "Save images and sequences.")


if __name__ == "__main__":
    unittest.main()
