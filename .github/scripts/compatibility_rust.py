"""Small Rust lexer/module resolver for exact fixture bodies, without syn/Go.

Comments and whitespace outside literals do not contribute to body identity.
Literal bytes do: changing whitespace inside a string changes its meaning.
Module declarations (including inline modules and #[path]) are followed from
the crate root, so an unrelated function with the same leaf name cannot pass.
"""

import hashlib
import json
from pathlib import PurePosixPath
import re


def tokens(source):
    result = []
    i = 0
    while i < len(source):
        start = i
        if source[i].isspace():
            i += 1
            continue
        if source.startswith("//", i):
            i = source.find("\n", i)
            if i < 0:
                break
            continue
        if source.startswith("/*", i):
            depth = 1
            i += 2
            while depth and i < len(source):
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
            if depth:
                raise ValueError("Unterminated Rust block comment")
            continue
        raw = re.match(r'(?:br|cr|r)(#*)"', source[i:])
        char = re.match(r"'(?:[^'\\\n]|\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|[^\n]))'", source[i:])
        if raw:
            terminator = '"' + raw[1]
            end = source.find(terminator, i + raw.end())
            if end < 0:
                raise ValueError("Unterminated Rust raw string")
            i = end + len(terminator)
        elif source[i] == '"':
            i += 1
            while i < len(source) and source[i] != '"':
                i += 2 if source[i] == "\\" else 1
            if i >= len(source):
                raise ValueError("Unterminated Rust string")
            i += 1
        elif char:
            i += char.end()
        else:
            word = re.match(r"[A-Za-z_][A-Za-z_0-9]*|[0-9][A-Za-z_0-9]*", source[i:])
            i += word.end() if word else 1
        result.append((source[start:i], start, i))
    return result


def closing(items, start):
    pairs = {"{": "}", "[": "]", "(": ")"}
    stack = []
    for index in range(start, len(items)):
        value = items[index][0]
        if value in pairs:
            stack.append(pairs[value])
        elif value in pairs.values():
            if not stack or stack.pop() != value:
                raise ValueError("Unbalanced Rust delimiters")
            if not stack:
                return index
    raise ValueError("Unclosed Rust delimiter")


def declarations(items):
    """Yield top-level module/function declarations, skipping other groups."""
    attrs = []
    i = 0
    while i < len(items):
        value = items[i][0]
        if value == "#" and i + 1 < len(items) and items[i + 1][0] == "[":
            end = closing(items, i + 1)
            attrs.append([token[0] for token in items[i + 2:end]])
            i = end + 1
            continue
        if value in ("fn", "mod") and i + 2 < len(items):
            kind, name = value, items[i + 1][0]
            j = i + 2
            while j < len(items) and items[j][0] not in ("{", ";"):
                if items[j][0] in ("(", "["):
                    j = closing(items, j)
                j += 1
            if j >= len(items):
                raise ValueError(f"Cannot find Rust body/declaration for {name}")
            end = closing(items, j) if items[j][0] == "{" else j
            yield kind, name, attrs, items[j + 1:end] if items[j][0] == "{" else None
            attrs = []
            i = end + 1
            continue
        if value in ("{", "[", "("):
            i = closing(items, i) + 1
            attrs = []
        else:
            if value == ";":
                attrs = []
            i += 1


def is_test(attrs):
    for attr in attrs:
        prefix = attr[:attr.index("(")] if "(" in attr else attr
        names = [value for value in prefix if re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", value)]
        if names and names[-1] == "test":
            return True
    return False


class Resolver:
    def __init__(self, read):
        self.read = read
        self.cache = {}

    def file(self, path):
        path = str(path)
        if path not in self.cache:
            self.cache[path] = tokens(self.read(path))
        return self.cache[path]

    def body(self, root, qualified, expected):
        path = PurePosixPath(root)
        module_dir = path.parent
        items = self.file(path)
        names = qualified.split("::")
        for number, name in enumerate(names):
            kind = "fn" if number == len(names) - 1 else "mod"
            found = [item for item in declarations(items) if item[:2] == (kind, name)]
            if len(found) != 1:
                raise ValueError(f"Evidence test does not exist uniquely: {qualified} ({path}, {name})")
            _, _, attrs, body = found[0]
            if kind == "fn":
                if str(path) != expected or body is None or not is_test(attrs):
                    raise ValueError(f"{qualified} is not a test defined in {expected} (resolved {path})")
                return body
            if body is not None:
                items = body
                module_dir /= name
                continue
            override = next((attr[2] for attr in attrs if attr[:2] == ["path", "="]), None)
            if override:
                path = path.parent / json.loads(override)
            else:
                flat = module_dir / (name + ".rs")
                nested = module_dir / name / "mod.rs"
                try:
                    self.file(flat)
                    path = flat
                except FileNotFoundError:
                    path = nested
            items = self.file(path)
            module_dir = path.parent if path.name == "mod.rs" else path.parent / path.stem
        raise ValueError(f"Could not extract {qualified}")


def digest(body):
    values = [token[0] for token in body]
    return hashlib.sha256(json.dumps(values, ensure_ascii=False, separators=(",", ":")).encode("utf-8")).hexdigest()
