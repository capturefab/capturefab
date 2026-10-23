#!/usr/bin/env python3
"""Validate documentation coverage against the current public source interfaces.

No compiled executable or third-party Python package is needed. The small Rust
reader handles attributes, literals, comments and balanced enum bodies. Feature
gates stay included: the website documents the union of supported builds.
"""
import argparse
import json
import re
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parents[1]
INTERNAL_RPC = {"Grab"}  # Worker-only acquisition, documented by public Frame.
RAW_STRING = re.compile(r'(?:br|r)(#*)"')
CHAR = re.compile(r"'(?:\\.|[^'\\\n])'")
IDENTIFIER = re.compile(r"[A-Za-z_][A-Za-z_0-9]*")


def rust_tokens(source):
    """Read enough Rust syntax to distinguish declarations from prose/literals."""
    tokens = []
    i = 0
    while i < len(source):
        if source[i].isspace():
            i += 1
        elif source.startswith("//", i):
            end = source.find("\n", i)
            i = len(source) if end < 0 else end + 1
        elif source.startswith("/*", i):
            depth = 1
            i += 2
            while i < len(source) and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("unterminated Rust block comment")
        elif (raw := RAW_STRING.match(source, i)):
            end_marker = '"' + raw[1]
            start = raw.end()
            end = source.find(end_marker, start)
            if end < 0:
                raise ValueError("unterminated Rust raw string")
            tokens.append(json.dumps(source[start:end]))
            i = end + len(end_marker)
        elif source[i] == '"':
            start = i
            i += 1
            while i < len(source):
                if source[i] == "\\":
                    i += 2
                elif source[i] == '"':
                    i += 1
                    break
                else:
                    i += 1
            else:
                raise ValueError("unterminated Rust string")
            tokens.append(source[start:i])
        elif source[i] == "'" and (char := CHAR.match(source, i)):
            tokens.append(char[0])
            i = char.end()
        elif (identifier := IDENTIFIER.match(source, i)):
            tokens.append(identifier[0])
            i = identifier.end()
        elif source.startswith("::", i):
            tokens.append("::")
            i += 2
        else:
            tokens.append(source[i])
            i += 1
    return tokens


def closing(tokens, start):
    pairs = {"(": ")", "[": "]", "{": "}"}
    expected = [pairs[tokens[start]]]
    for i in range(start + 1, len(tokens)):
        token = tokens[i]
        if token in pairs:
            expected.append(pairs[token])
        elif token in pairs.values():
            if not expected or token != expected.pop():
                raise ValueError("unbalanced Rust delimiters")
            if not expected:
                return i
    raise ValueError("unterminated Rust delimiter")


def split_items(tokens):
    start = i = 0
    while i < len(tokens):
        if tokens[i] in ("(", "[", "{"):
            i = closing(tokens, i)
        elif tokens[i] == ",":
            if i > start:
                yield tokens[start:i]
            start = i + 1
        i += 1
    if start < len(tokens):
        yield tokens[start:]


def attributes(tokens):
    result = []
    i = 0
    while tokens[i:i + 2] == ["#", "["]:
        end = closing(tokens, i + 1)
        result.append(tokens[i + 2:end])
        i = end + 1
    return result, tokens[i:]


def attr_values(attrs, family, key):
    values = []
    for attr in attrs:
        if attr[:2] != [family, "("]:
            continue
        for i, token in enumerate(attr):
            if token != key or attr[i + 1:i + 2] != ["="]:
                continue
            value = attr[i + 2]
            if value == "[":
                values.extend(json.loads(t) for t in attr[i + 3:closing(attr, i + 2)] if t.startswith('"'))
            elif value.startswith('"'):
                values.append(json.loads(value))
            else:
                values.append(value)
    return values


def enum_variants(source, name):
    tokens = rust_tokens(source) if isinstance(source, str) else source
    for i in range(len(tokens) - 2):
        if tokens[i:i + 3] == ["enum", name, "{"]:
            body = tokens[i + 3:closing(tokens, i + 2)]
            variants = []
            for item in split_items(body):
                attrs, rest = attributes(item)
                if not rest or not re.fullmatch(r"[A-Za-z_]\w*", rest[0]):
                    raise ValueError(f"cannot read {name} enum variant")
                variants.append({"name": rest[0], "attrs": attrs, "body": rest[1:]})
            return variants
    raise ValueError(f"enum {name} not found")


def struct_fields(source, name):
    tokens = rust_tokens(source) if isinstance(source, str) else source
    for i in range(len(tokens) - 2):
        if tokens[i:i + 3] == ["struct", name, "{"]:
            return list(split_items(tokens[i + 3:closing(tokens, i + 2)]))
    raise ValueError(f"struct {name} not found")


def attr_options(attrs, family):
    """Read top-level attribute options, excluding text inside builder calls."""
    result = {}
    for attr in attrs:
        if attr[:2] != [family, "("]:
            continue
        for item in split_items(attr[2:-1]):
            if item:
                result[item[0]] = item[2:] if item[1:2] == ["="] else []
    return result


def type_name(tokens):
    while tokens[:2] in (["Option", "<"], ["Box", "<"]):
        if tokens[-1] != ">":
            raise ValueError("cannot read wrapped argument type")
        tokens = tokens[2:-1]
    if not tokens or not all(t == "::" or re.fullmatch(r"[A-Za-z_]\w*", t) for t in tokens):
        raise ValueError(f"unsupported argument type: {''.join(tokens)}")
    return "".join(tokens)


def snake_case(name):
    name = re.sub(r"([A-Z]+)([A-Z][a-z])", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", name).lower()


def subcommand_type(body):
    if not body or body[0] != "{":
        return None
    for field in split_items(body[1:-1]):
        attrs, rest = attributes(field)
        if any(attr == ["command", "(", "subcommand", ")"] for attr in attrs):
            if ":" not in rest:
                raise ValueError("cannot read subcommand field type")
            return type_name(rest[rest.index(":") + 1:])
    return None


def command_paths(root, with_arguments=False):
    """Return canonical command paths and visible aliases at their declarations."""
    cache, tokens = {}, {}

    def source(path):
        if path not in cache:
            cache[path] = path.read_text(encoding="utf-8")
        return cache[path]

    def resolve(typename, current, kind="enum"):
        if typename.startswith("crate::"):
            parts = typename.split("::")[1:]
            return root / "src" / Path(*parts[:-1]).with_suffix(".rs"), parts[-1]
        name = typename.split("::")[-1]
        candidates = [current] + sorted((root / "src" / "cli").glob("*.rs"))
        matches = [p for p in candidates if re.search(rf"\b{kind}\s+{re.escape(name)}\s*\{{", source(p))]
        if current in matches:
            return current, name
        if len(matches) != 1:
            raise ValueError(f"cannot resolve {'subcommand enum' if kind == 'enum' else 'argument struct'} {typename} from {current}")
        return matches[0], name

    result = {"help"}
    arguments = {}

    def fields_arguments(fields, path, ancestors):
        found = set()
        for field in fields:
            attrs, rest = attributes(field)
            options = attr_options(attrs, "arg")
            command = attr_options(attrs, "command")
            if "subcommand" in command or "skip" in options or ("hide" in options and options["hide"] != ["false"]):
                continue
            if "flatten" in command or "flatten" in options:
                if ":" not in rest:
                    raise ValueError("cannot read flattened argument type")
                child = type_name(rest[rest.index(":") + 1:])
                found.update(flatten_arguments(child, path, ancestors))
                continue
            if ":" not in rest:
                raise ValueError(f"cannot read argument field: {rest}")
            name = rest[rest.index(":") - 1]
            for key, prefix in (("long", "--"), ("short", "-")):
                if key not in options:
                    continue
                value = options[key]
                if value:
                    spelling = value[0]
                    spelling = json.loads(spelling) if spelling.startswith('"') else spelling.strip("'")
                else:
                    spelling = name.replace("_", "-") if key == "long" else name[0]
                found.add(prefix + spelling)
            for key, prefix in (("visible_alias", "--"), ("visible_aliases", "--"), ("visible_short_alias", "-"), ("visible_short_aliases", "-")):
                for value in attr_values(attrs, "arg", key):
                    found.add(prefix + value.strip("'"))
            if "long" not in options and "short" not in options:
                label = options.get("value_name") or options.get("id")
                found.add(json.loads(label[0]) if label else name.upper())
        return found

    def flatten_arguments(typename, path, ancestors):
        child_path, name = resolve(typename, path, "struct")
        identity = (child_path, name)
        if identity in ancestors:
            raise ValueError(f"recursive argument struct {name}")
        if child_path not in tokens:
            tokens[child_path] = rust_tokens(source(child_path))
        fields = struct_fields(tokens[child_path], name)
        return fields_arguments(fields, child_path, ancestors | {identity})

    def visit(path, enum, prefix, ancestors):
        identity = (path, enum)
        if identity in ancestors:
            raise ValueError(f"recursive subcommand enum {enum}")
        if path not in tokens:
            tokens[path] = rust_tokens(source(path))
        for variant in enum_variants(tokens[path], enum):
            attrs = variant["attrs"]
            if "true" in attr_values(attrs, "command", "hide"):
                continue
            names = attr_values(attrs, "command", "name") or [snake_case(variant["name"]).replace("_", "-")]
            if len(names) != 1:
                raise ValueError("command has more than one canonical name")
            canonical = " ".join(prefix + names)
            result.add(canonical)
            if with_arguments:
                body = variant["body"]
                if body and body[0] == "{":
                    arguments[canonical] = fields_arguments(list(split_items(body[1:-1])), path, set())
                elif body and body[0] == "(":
                    arguments[canonical] = flatten_arguments(type_name(body[1:-1]), path, set())
                else:
                    arguments[canonical] = set()
            for key in ("visible_alias", "visible_aliases"):
                for alias in attr_values(attrs, "command", key):
                    result.add(" ".join(prefix + [alias]))
            child = subcommand_type(variant["body"])
            if child:
                child_path, child_enum = resolve(child, path)
                visit(child_path, child_enum, prefix + names, ancestors | {identity})
    visit(root / "src/cli.rs", "Command", [], set())
    if with_arguments:
        arguments[""] = flatten_arguments("Cli", root / "src/cli.rs", set())
        return result, arguments
    return result


def tool_names(root):
    tokens = rust_tokens((root / "src/tools/mod.rs").read_text(encoding="utf-8"))
    try:
        start = tokens.index("CATALOG")
        start = tokens.index("=", start) + 1
        if tokens[start] == "&":
            start += 1
        entries = list(split_items(tokens[start + 1:closing(tokens, start)]))
    except (ValueError, KeyError, IndexError) as error:
        raise ValueError("cannot read tools CATALOG") from error
    names, modules = set(), {}
    for entry in entries:
        if len(entry) != 3 or entry[1] != "::":
            raise ValueError(f"unsupported CATALOG entry: {entry}")
        module, _, const = entry
        if module not in modules:
            modules[module] = rust_tokens((root / f"src/tools/{module}.rs").read_text(encoding="utf-8"))
        tokens = modules[module]
        try:
            i = next(i for i in range(len(tokens) - 2) if tokens[i:i + 2] == ["const", const])
            i = tokens.index("=", i) + 1
            if tokens[i:i + 2] == ["ToolSpec", "{"]:
                end = closing(tokens, i + 1)
                fields = list(split_items(tokens[i + 2:end]))
                value = next(field[2] for field in fields if field[:2] == ["name", ":"])
            else:
                if tokens[i + 1] != "(":
                    raise ValueError("tool specification must be a struct or factory call")
                value = tokens[i + 2]
            name = json.loads(value)
        except (ValueError, IndexError, StopIteration) as error:
            raise ValueError(f"cannot read ToolSpec {module}::{const}") from error
        if name in names:
            raise ValueError(f"duplicate tool name: {name}")
        names.add(name)
    return names


def rpc_names(root):
    return {
        (attr_values(v["attrs"], "serde", "rename") or [snake_case(v["name"])])[0]
        for v in enum_variants((root / "src/session.rs").read_text(encoding="utf-8"), "SessionCommand")
        if v["name"] not in INTERNAL_RPC
    }


class DocPage(HTMLParser):
    """Keep element boundaries so a heading needs an actual explanation after it."""
    VOID = {"area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"}
    EXCLUDE = {"a", "nav", "button", "script", "style", "h1", "h2", "h3", "h4", "h5", "h6"}

    def __init__(self, source):
        super().__init__(convert_charrefs=True)
        self.root = {"tag": "root", "attrs": {}, "children": [], "parent": None}
        self.stack = [self.root]
        self.ids = {}
        self.feed(source)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        node = {"tag": tag, "attrs": attrs, "children": [], "parent": self.stack[-1]}
        self.stack[-1]["children"].append(node)
        if attrs.get("id"):
            if attrs["id"] in self.ids:
                raise ValueError(f"duplicate documentation anchor: {attrs['id']}")
            self.ids[attrs["id"]] = node
        if tag not in self.VOID:
            self.stack.append(node)

    def handle_endtag(self, tag):
        for i in range(len(self.stack) - 1, 0, -1):
            if self.stack[i]["tag"] == tag:
                self.stack = self.stack[:i]
                break

    def handle_data(self, data):
        self.stack[-1]["children"].append(data)

    def prose(self, node):
        if isinstance(node, str):
            return node
        if node["tag"] in self.EXCLUDE or node["attrs"].get("aria-hidden") == "true":
            return ""
        return " ".join(self.prose(child) for child in node["children"])

    def has_explanation(self, anchor):
        text = " ".join(self.prose(node) for node in self.section_nodes(anchor))
        return len(text.strip()) >= 10 and len(text.split()) >= 2

    def section_nodes(self, anchor):
        node = self.ids.get(anchor)
        if node is None:
            return []
        if re.fullmatch("h[1-6]", node["tag"]):
            siblings = node["parent"]["children"]
            start = next(i for i, sibling in enumerate(siblings) if sibling is node) + 1
            parts = []
            for sibling in siblings[start:]:
                if isinstance(sibling, dict) and re.fullmatch("h[1-6]", sibling["tag"]) and sibling["tag"] <= node["tag"]:
                    break
                parts.append(sibling)
            return parts
        return [node]

    def code_text(self, anchor):
        def text(node, inside=False):
            if isinstance(node, str):
                return node if inside else ""
            if node["tag"] in {"a", "nav", "button", "script", "style"} or node["attrs"].get("aria-hidden") == "true":
                return ""
            return " ".join(text(child, inside or node["tag"] == "code") for child in node["children"])
        return " ".join(text(node) for node in self.section_nodes(anchor))


def argument_present(text, token):
    """Match complete flag spellings/positional labels, never a longer flag."""
    boundary = r"[A-Za-z0-9_-]" if token.startswith("-") else r"[A-Za-z0-9_]"
    return re.search(rf"(?<!{boundary}){re.escape(token)}(?!{boundary})", text) is not None


def compare_names(label, actual, documented, errors):
    missing = actual - set(documented)
    stale = set(documented) - actual
    if missing:
        errors.append(f"{label}: missing documentation: {', '.join(sorted(missing))}")
    if stale:
        errors.append(f"{label}: stale documentation entries: {', '.join(sorted(stale))}")
    return {"documented": len(actual & set(documented)), "total": len(actual)}


def validate(root=ROOT):
    root = Path(root)
    cli = json.loads((root / "docs/cli-coverage.json").read_text(encoding="utf-8"))
    desktop = json.loads((root / "docs/desktop-coverage.json").read_text(encoding="utf-8"))
    if cli.get("schema_version") != 1 or desktop.get("version") != 1:
        raise ValueError("unsupported documentation coverage inventory version")
    errors, counts, pages = [], {}, {}

    def target(url, label):
        if not isinstance(url, str):
            errors.append(f"{label}: documentation target must be a string")
            return
        parsed = urlsplit(url)
        path = root / "web" / unquote(parsed.path)
        if parsed.scheme or parsed.netloc or parsed.query or not parsed.fragment or not parsed.path.endswith(".html") or ".." in Path(parsed.path).parts or Path(parsed.path).is_absolute():
            errors.append(f"{label}: invalid local documentation target: {url}")
            return
        if not path.is_file():
            errors.append(f"{label}: missing documentation page: {url}")
            return
        if path not in pages:
            pages[path] = DocPage(path.read_text(encoding="utf-8"))
        if not pages[path].has_explanation(unquote(parsed.fragment)):
            errors.append(f"{label}: missing or unexplained documentation anchor: {url}")

    commands, arguments = command_paths(root, with_arguments=True)
    for label, actual in (("commands", commands), ("tools", tool_names(root)), ("rpc", rpc_names(root))):
        documented = cli.get(label)
        if not isinstance(documented, dict):
            raise ValueError(f"{label}: expected coverage mapping")
        counts[label] = compare_names(label, actual, documented, errors)
        for key, url in documented.items():
            target(url, f"{label} {key}")

    argument_total = argument_documented = 0
    for command, expected in arguments.items():
        if not expected:
            continue
        # Globals have one dedicated reference. Local options must appear in
        # their own command block even if the same spelling is also global.
        url = cli["commands"].get(command) if command else "reference.html#cli-global-options"
        argument_total += len(expected)
        if not url:
            continue  # The missing command mapping is already reported above.
        label = f"arguments {command or '(global)'}"
        target(url, label)
        parsed = urlsplit(url)
        path = root / "web" / unquote(parsed.path)
        page = pages.get(path)
        code = page.code_text(unquote(parsed.fragment)) if page else ""
        absent = [token for token in sorted(expected) if not argument_present(code, token)]
        argument_documented += len(expected) - len(absent)
        if absent:
            errors.append(f"{label}: missing code reference: {', '.join(absent)}")
    counts["argument_spellings"] = {"documented": argument_documented, "total": argument_total}

    features = desktop.get("features")
    if not isinstance(features, dict) or not features:
        raise ValueError("desktop: expected nonempty features mapping")
    represented_actions, represented_routes = set(), set()
    valid_features = 0
    for key, feature in features.items():
        before = len(errors)
        if not isinstance(feature, dict) or not isinstance(feature.get("source"), str) or not isinstance(feature.get("terms"), list) or not feature["terms"]:
            errors.append(f"desktop {key}: expected source, nonempty terms and url")
            continue
        path = root / feature["source"]
        if Path(feature["source"]).is_absolute() or ".." in Path(feature["source"]).parts or not path.is_file():
            errors.append(f"desktop {key}: missing or invalid source: {feature['source']}")
            continue
        source = path.read_text(encoding="utf-8")
        for term in feature["terms"]:
            if not isinstance(term, str) or not term or term not in source:
                errors.append(f"desktop {key}: source term no longer exists: {term!r}")
            elif feature["source"] == "src/gui/keys.rs":
                represented_actions.update(re.findall(r"\bAction::(\w+)\b", term))
            elif feature["source"] == "src/remote.rs":
                represented_routes.update(re.findall(r"/(?:api/[a-z0-9_.-]+|health|metrics)", term))
        target(feature.get("url"), f"desktop {key}")
        if len(errors) == before:
            valid_features += 1
    actions = {v["name"] for v in enum_variants((root / "src/gui/keys.rs").read_text(encoding="utf-8"), "Action")}
    routes = set(re.findall(r'"(/(?:api/[a-z0-9_.-]+|health|metrics))"', (root / "src/remote.rs").read_text(encoding="utf-8")))
    counts["keyboard_actions"] = compare_names("keyboard actions", actions, represented_actions, errors)
    counts["http_routes"] = compare_names("HTTP routes", routes, represented_routes, errors)
    counts["desktop_features"] = {"documented": valid_features, "total": len(features)}
    return {"ok": not errors, "coverage": counts, "errors": errors}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root (used by tests)")
    parser.add_argument("--json", action="store_true", help="print the report as JSON")
    args = parser.parse_args()
    try:
        report = validate(args.root)
    except (OSError, ValueError, KeyError, TypeError) as error:
        report = {"ok": False, "coverage": {}, "errors": [str(error)]}
    if args.json:
        print(json.dumps(report, indent=2))
    else:
        for label, count in report["coverage"].items():
            print(f"{label}: {count['documented']}/{count['total']} documented")
        for error in report["errors"]:
            print(error, file=sys.stderr)
        if report["ok"]:
            print("Documentation coverage passed; all targets have explanatory content.")
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
