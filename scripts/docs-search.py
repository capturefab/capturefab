#!/usr/bin/env python3
"""Generate the local documentation search index from the published HTML."""
import argparse
import json
import re
from html.parser import HTMLParser
from pathlib import Path

WEB = Path(__file__).resolve().parents[1] / "web"
VOID_TAGS = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}


def clean(parts):
    return re.sub(r"\s+", " ", " ".join(parts)).strip()


class Documentation(HTMLParser):
    """Index visible article text, including command examples and captions."""
    def __init__(self, page="docs.html"):
        super().__init__(convert_charrefs=True)
        self.page = page
        self.stack = []
        self.current = None
        self.documents = []
        self.errors = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if "data-doc-section" in attrs:
            if self.current is not None:
                self.errors.append("documentation sections must not be nested")
            else:
                self.current = dict(
                    id=attrs.get("id", ""),
                    title=attrs.get("data-doc-title", ""),
                    category=attrs.get("data-doc-category", "Documentation"),
                    tags=[value.strip() for value in attrs.get("data-doc-tags", "").split(",") if value.strip()],
                    heading=[], summary=[], body=[], depth=len(self.stack),
                )
        frame = dict(tag=tag, ignored=tag in {"script", "style", "button"} or attrs.get("aria-hidden") == "true",
                     heading=tag == "h2", summary="doc-summary" in attrs.get("class", "").split())
        if tag not in VOID_TAGS:
            self.stack.append(frame)

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)
        if tag not in VOID_TAGS:
            self.handle_endtag(tag)

    def handle_endtag(self, tag):
        if tag in VOID_TAGS:
            return
        matching = next((i for i in range(len(self.stack) - 1, -1, -1) if self.stack[i]["tag"] == tag), None)
        if matching is None:
            return
        self.stack = self.stack[:matching]
        if self.current is not None and len(self.stack) == self.current["depth"]:
            self.finish_section()

    def handle_data(self, data):
        if self.current is None or any(frame["ignored"] for frame in self.stack):
            return
        self.current["body"].append(data)
        if any(frame["heading"] for frame in self.stack):
            self.current["heading"].append(data)
        if any(frame["summary"] for frame in self.stack):
            self.current["summary"].append(data)

    def finish_section(self):
        section = self.current
        self.current = None
        heading = clean(section.pop("heading"))
        section.pop("depth")
        section["summary"] = clean(section["summary"])
        section["body"] = clean(section["body"])
        identity = section["id"]
        if not re.fullmatch(r"[a-z][a-z0-9-]*", identity):
            self.errors.append(f"invalid documentation section id: {identity!r}")
        if any(item["id"] == identity for item in self.documents):
            self.errors.append(f"duplicate documentation section: {identity}")
        if not section["title"] or section["title"] != heading:
            self.errors.append(f"{identity}: data-doc-title must match its h2")
        if not section["summary"]:
            self.errors.append(f"{identity}: missing .doc-summary")
        section["url"] = f"{self.page}#{identity}"
        self.documents.append(section)


def index(web=WEB):
    documents = []
    errors = []
    # Keep the task guides first when equally relevant results have equal scores.
    pages = [web / "docs.html", *sorted(path for path in web.glob("*.html") if path.name != "docs.html")]
    for path in pages:
        parser = Documentation(path.name)
        parser.feed(path.read_text(encoding="utf-8"))
        parser.close()
        if parser.current is not None:
            parser.errors.append("unclosed documentation section")
        errors.extend(f"{path.name}: {error}" for error in parser.errors)
        documents.extend(parser.documents)
    if not documents:
        errors.append("no documentation sections found")
    if errors:
        raise ValueError("; ".join(errors))
    return dict(schema_version=1, documents=documents)


def generated(web=WEB):
    return json.dumps(index(web), ensure_ascii=False, indent=2) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--web", type=Path, default=WEB)
    parser.add_argument("--check", action="store_true", help="fail when the index is missing or stale")
    args = parser.parse_args()
    try:
        text = generated(args.web)
    except (ValueError, OSError) as error:
        parser.exit(1, f"docs-search: {error}\n")
    path = args.web / "search-index.json"
    if args.check:
        if not path.is_file() or path.read_text(encoding="utf-8") != text:
            parser.exit(1, "docs-search: search-index.json is stale; run scripts/docs-search.py\n")
    else:
        path.write_text(text, encoding="utf-8")


if __name__ == "__main__":
    main()
