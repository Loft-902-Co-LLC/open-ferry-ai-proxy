#!/usr/bin/env python3
"""Validate the buildless project site, offline and with the standard library.

HTML is intentionally explicit: even tags HTML permits implicitly closing must
be closed here, so a forgotten end tag cannot silently change the page layout.
"""

import argparse
from html import unescape
from html.parser import HTMLParser
from pathlib import Path
import re
import sys
import tempfile
from urllib.parse import unquote, urlsplit


VOID = frozenset("area base br col embed hr img input link meta param source track wbr".split())
REPO = "github.com/loft-902-co-llc/open-ferry-ai-proxy"
URL_ATTRIBUTES = frozenset("href src poster action formaction data cite background".split())
CSS_URL = re.compile(r"url\(\s*['\"]?([^'\"\s)]+)['\"]?\s*\)", re.I)
CSS_IMPORT = re.compile(r"@import\s+(?:url\(\s*)?['\"]?([^'\"\s);]+)", re.I)


class Page(HTMLParser):
    def __init__(self, name):
        super().__init__(convert_charrefs=True)
        self.name = name
        self.errors = []
        self.stack = []
        self.ids = set()
        self.urls = []
        self.lang = False
        self.title = False
        self.viewport = False
        self.h1 = 0

    def fail(self, rule, message):
        self.errors.append(f"{self.name}:{self.getpos()[0]} [{rule}] {message}")

    def handle_starttag(self, tag, attrs):
        names = [name for name, _ in attrs]
        if len(set(names)) != len(names):
            self.fail("html", "duplicate attribute")
        attrs = dict(attrs)
        if tag == "html":
            self.lang = bool((attrs.get("lang") or "").strip())
        if tag == "h1":
            self.h1 += 1
        if tag == "meta" and (attrs.get("name") or "").lower() == "viewport":
            self.viewport = bool((attrs.get("content") or "").strip())
        if "id" in attrs:
            identifier = attrs["id"]
            if not identifier or identifier in self.ids:
                self.fail("duplicate-id", f"empty or duplicate id: {identifier!r}")
            self.ids.add(identifier)
        if tag == "img" and "alt" not in attrs:
            self.fail("alt", "img needs alt (empty alt is valid for decoration)")
        if tag == "base":
            self.fail("relative", "base elements break project-relative URLs")
        resource = tag == "script" or (tag == "link" and "stylesheet" in (attrs.get("rel") or "").lower().split())
        for name, value in attrs.items():
            if name in URL_ATTRIBUTES and value:
                self.urls.append((value, resource and name in {"src", "href"}))
            if name == "srcset" and value:
                for entry in value.split(","):
                    if entry.strip():
                        self.urls.append((entry.strip().split()[0], False))
            if name == "style" and value:
                self.urls.extend((url, False) for url in CSS_URL.findall(value))
                self.urls.extend((url, True) for url in CSS_IMPORT.findall(value))
        if tag not in VOID:
            self.stack.append(tag)

    def handle_startendtag(self, tag, attrs):
        self.handle_starttag(tag, attrs)
        if tag not in VOID:
            self.handle_endtag(tag)

    def handle_endtag(self, tag):
        if not self.stack or self.stack[-1] != tag:
            self.fail("html", f"unexpected </{tag}>; open tags: {self.stack}")
            return
        self.stack.pop()

    def handle_data(self, data):
        if self.stack and self.stack[-1] == "title" and data.strip():
            self.title = True
        if self.stack and self.stack[-1] == "style":
            self.urls.extend((url, False) for url in CSS_URL.findall(data))
            self.urls.extend((url, True) for url in CSS_IMPORT.findall(data))
        elif "<" in data and not (self.stack and self.stack[-1] in {"script", "pre", "code"}):
            self.fail("html", "unparsed '<' (escape literal markup as &lt;)")

    def finish(self):
        self.close()
        if self.stack:
            self.fail("html", f"unclosed tags: {self.stack}")
        for rule, present, message in (
            ("lang", self.lang, "html needs a language"),
            ("title", self.title, "page needs a nonempty title"),
            ("viewport", self.viewport, "page needs a viewport meta"),
            ("h1", self.h1 == 1, f"expected exactly one h1, found {self.h1}"),
        ):
            if not present:
                self.fail(rule, message)


def markdown_ids(path):
    """GitHub heading slugs, including duplicate suffixes and explicit HTML ids.

    Handles ATX/setext headings outside fenced blocks. Inline code, emphasis,
    links and HTML are stripped before GitHub's punctuation/space rules.
    """
    text = path.read_text(encoding="utf-8")
    identifiers = set(re.findall(r"\b(?:id|name)=[\"']([^\"']+)[\"']", text))
    used = set()
    fence = None
    previous = ""
    for line in text.splitlines():
        marker = re.match(r"^\s{0,3}(`{3,}|~{3,})", line)
        if marker:
            if fence is None:
                fence = marker[1][0]
            elif marker[1][0] == fence:
                fence = None
            previous = ""
            continue
        if fence:
            continue
        heading = re.match(r"^\s{0,3}#{1,6}\s+(.+?)(?:\s+#+\s*)?$", line)
        label = heading[1] if heading else None
        if label is None and previous.strip() and re.fullmatch(r"\s{0,3}(?:=+|-+)\s*", line):
            label = previous.strip()
        previous = line
        if label is None:
            continue
        label = re.sub(r"!?\[([^\]]*)\]\([^)]*\)", r"\1", label)
        label = re.sub(r"<[^>]*>", "", label)
        label = unescape(re.sub(r"[`*_~]", "", label)).lower()
        slug = "".join(c for c in label if c.isalnum() or c in " _-").replace(" ", "-")
        candidate, suffix = slug, 0
        while candidate in used:
            suffix += 1
            candidate = f"{slug}-{suffix}"
        used.add(candidate)
        identifiers.add(candidate)
    return identifiers


def within(path, root):
    return path == root or root in path.parents


def check(site, repo):
    site, repo = site.resolve(), repo.resolve()
    errors, pages = [], {}
    for path in sorted(site.rglob("*.html")):
        page = Page(path.relative_to(repo) if within(path, repo) else path.name)
        try:
            source = path.read_text(encoding="utf-8")
            page.feed(source)
            page.finish()
        except (OSError, UnicodeError, ValueError) as error:
            page.fail("html", str(error))
        errors.extend(page.errors)
        pages[path.resolve()] = page
    if not pages:
        errors.append("[html] site has no HTML pages")

    def fail(origin, rule, message):
        label = origin.relative_to(repo) if within(origin, repo) else origin.name
        errors.append(f"{label} [{rule}] {message}")

    def check_url(origin, url, resource=False):
        try:
            parsed = urlsplit(unescape(url))
        except ValueError:
            fail(origin, "url", f"invalid URL: {url}")
            return
        if parsed.scheme.lower() == "http":
            fail(origin, "https", f"insecure URL: {url}")
        if resource and (parsed.scheme or parsed.netloc):
            fail(origin, "external-resource", f"script/stylesheet must be local: {url}")
        if parsed.netloc:
            host_path = (parsed.netloc + parsed.path).lower()
            if host_path.startswith(REPO + "/blob/main/") or host_path.startswith(REPO + "/tree/main/"):
                kind, relative = parsed.path.split("/main/", 1)
                target = (repo / unquote(relative)).resolve()
                if not within(target, repo) or not target.exists():
                    fail(origin, "repo-path", f"repository target missing: {relative}")
                elif ("/blob" in kind and not target.is_file()) or ("/tree" in kind and not target.is_dir()):
                    fail(origin, "repo-path", f"blob/tree target kind mismatch: {relative}")
                elif parsed.fragment and target.suffix.lower() == ".md" and unquote(parsed.fragment) not in markdown_ids(target):
                    fail(origin, "repo-fragment", f"repository anchor missing: {url}")
            return
        if parsed.scheme:
            if parsed.scheme not in {"https", "mailto", "tel", "data"}:
                fail(origin, "url", f"unsupported scheme: {parsed.scheme}")
            return
        if parsed.path.startswith("/"):
            fail(origin, "relative", f"use a project-relative URL: {url}")
            return
        target = (origin.parent / unquote(parsed.path)).resolve() if parsed.path else origin.resolve()
        if target.is_dir():
            target = target / "index.html"
        if not within(target, site) or not target.is_file():
            fail(origin, "relative", f"relative target missing or outside site: {url}")
        elif parsed.fragment:
            if target not in pages or unquote(parsed.fragment) not in pages[target].ids:
                fail(origin, "fragment", f"fragment missing: {url}")

    for path, page in pages.items():
        for url, resource in page.urls:
            check_url(path, url, resource)
    for path in sorted(site.rglob("*")):
        if path.suffix.lower() not in {".html", ".css", ".js", ".svg"} or not path.is_file():
            continue
        source = path.read_text(encoding="utf-8")
        # SVG's required namespace is an identifier, never a fetched HTTP URL.
        without_namespace = source.replace('xmlns="http://www.w3.org/2000/svg"', "") if path.suffix.lower() == ".svg" else source
        if re.search(r"http://", without_namespace, re.I):
            fail(path, "https", "http:// URL found; use https")
        if path.suffix.lower() == ".css":
            for url in CSS_URL.findall(source):
                check_url(path, url)
            for url in CSS_IMPORT.findall(source):
                check_url(path, url, True)
    return errors


def self_test():
    good = '<!doctype html><html lang="en"><head><title>Test</title><meta name="viewport" content="width=device-width"></head><body><h1 id="top">Test</h1>{}</body></html>'
    cases = [
        ("unclosed tag", "html", good.format("<div>")),
        ("mismatched nesting", "html", good.format("<div><p></div></p>")),
        ("incomplete markup", "html", good.format('<a href="unfinished')),
        ("duplicate id", "duplicate-id", good.format('<p id="top">Again</p>')),
        ("missing link", "relative", good.format('<a href="missing.html">Go</a>')),
        ("missing asset", "relative", good.format('<img src="missing.png" alt="Sample">')),
        ("missing local fragment", "fragment", good.format('<a href="#absent">Go</a>')),
        ("missing other-page fragment", "fragment", good.format('<a href="other.html#absent">Go</a>')),
        ("missing repo file", "repo-path", good.format('<a href="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/blob/main/missing.md">Go</a>')),
        ("missing repo directory", "repo-path", good.format('<a href="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/tree/main/missing">Go</a>')),
        ("missing repo anchor", "repo-fragment", good.format('<a href="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/blob/main/README.md#absent">Go</a>')),
        ("missing alt", "alt", good.format('<img src="asset.svg">')),
        ("missing language", "lang", good.format("").replace(' lang="en"', "")),
        ("missing title", "title", good.format("").replace('<title>Test</title>', "")),
        ("empty title", "title", good.format("").replace('>Test</title>', '></title>')),
        ("missing viewport", "viewport", good.format("").replace('<meta name="viewport" content="width=device-width">', "")),
        ("missing h1", "h1", good.format("").replace('<h1 id="top">Test</h1>', "")),
        ("multiple h1", "h1", good.format('<h1>Again</h1>')),
        ("insecure URL", "https", good.format('<a href="http://example.invalid">Go</a>')),
        ("external script", "external-resource", good.format('<script src="https://example.invalid/code.js"></script>')),
        ("protocol-relative script", "external-resource", good.format('<script src="//example.invalid/code.js"></script>')),
        ("external stylesheet", "external-resource", good.format('<link rel="stylesheet" href="https://example.invalid/style.css">')),
        ("root-relative URL", "relative", good.format('<a href="/index.html">Go</a>')),
    ]
    with tempfile.TemporaryDirectory(prefix="open-ferry-site-check-") as scratch:
        root = Path(scratch)
        site = root / "site"
        site.mkdir()
        (root / "README.md").write_text("# Heading\n## Heading\n## Heading\n", encoding="utf-8", newline="\n")
        (site / "other.html").write_text(good.format(""), encoding="utf-8", newline="\n")
        (site / "asset.svg").write_text('<svg xmlns="http://www.w3.org/2000/svg"></svg>', encoding="utf-8", newline="\n")
        for label, rule, source in cases:
            (site / "index.html").write_text(source, encoding="utf-8", newline="\n")
            if not any(f"[{rule}]" in error for error in check(site, root)):
                raise AssertionError(f"rule did not fire: {label}")
        positive = good.format('<a href="other.html#top">Go</a><img src="asset.svg" alt=""><a href="https://github.com/Loft-902-Co-LLC/open-ferry-ai-proxy/blob/main/README.md#heading-2">Duplicate heading</a>')
        (site / "index.html").write_text(positive, encoding="utf-8", newline="\n")
        assert not check(site, root), check(site, root)
        css_cases = [
            ("missing CSS asset", "relative", 'body { background: url("absent.png"); }'),
            ("external CSS import", "external-resource", '@import "https://example.invalid/theme.css";'),
            ("unquoted external CSS import", "external-resource", '@import url(https://example.invalid/theme.css);'),
            ("insecure CSS URL", "https", 'body { background: url("http://example.invalid/image.png"); }'),
        ]
        for label, rule, source in css_cases:
            (site / "styles.css").write_text(source, encoding="utf-8", newline="\n")
            if not any(f"[{rule}]" in error for error in check(site, root)):
                raise AssertionError(f"rule did not fire: {label}")
    print(f"Self-test passed: {len(cases) + len(css_cases)} bad samples rejected; valid cross-page links, alt and duplicate heading slugs accepted.")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()
    if args.self_test:
        self_test()
        return 0
    repo = Path(__file__).resolve().parents[2]
    errors = check(repo / "site", repo)
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print("Site check passed: HTML, local URLs, fragments, repository paths/Markdown anchors, alt text, page metadata and local scripts/stylesheets.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
