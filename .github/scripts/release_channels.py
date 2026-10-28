"""Release channel policy and CLI operations. Never builds or signs an image.

Run only from release-channels.yml; tests replace subprocess.run with fake
CLIs. Inputs arrive as environment variables.
"""

from datetime import datetime, timezone
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


POINTER = "channel-preview"
IMAGE = "ghcr.io/loft-902-co-llc/open-ferry"
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z.-]+))?")


def version_key(value):
    """Updater grammar, bounded for VERSION, with SemVer prerelease checks."""
    match = VERSION.fullmatch(value)
    if not match or len(value.encode("utf-8")) + 1 > 64:
        raise ValueError("Version must be MAJOR.MINOR.PATCH with an optional SemVer prerelease; at most 63 ASCII bytes")
    major, minor, patch, pre = match.groups()
    parts = []
    if pre is not None:
        for part in pre.split("."):
            if not part or (part.isdigit() and len(part) > 1 and part[0] == "0"):
                raise ValueError("Invalid SemVer prerelease identifier")
            # Numeric identifiers precede nonnumeric; stable follows all pre.
            parts.append((0, int(part)) if part.isdigit() else (1, part))
    return (int(major), int(minor), int(patch), pre is None, tuple(parts))


def withdrawn(release):
    return (release.get("name") or "").casefold().rstrip().endswith(" (withdrawn)")


def published(release):
    return not release["draft"] and bool(release.get("published_at"))


def release_version(release):
    tag = release["tag_name"]
    if not tag.startswith("v"):
        return None
    try:
        version_key(tag[1:])
    except ValueError:
        return None
    return tag[1:]


def newest(releases):
    return max(releases, key=lambda r: version_key(release_version(r)), default=None)


def require_release(releases, version):
    for release in releases:
        if release["tag_name"] == "v" + version:
            return release
    raise ValueError("Release does not exist")


def decide(operation, version, releases, latest, pointer):
    """Pure decisions; API's latest is authoritative, never publication order."""
    version_key(version)
    target = require_release(releases, version)
    if not published(target):
        raise ValueError("Release must be published, not a draft")
    if operation == "publish":
        if withdrawn(target):
            return {}
        # Events can be delayed or dropped from GitHub's pending slot. Any
        # surviving publication reconciles all currently good releases.
        candidate = newest(r for r in releases if published(r) and not withdrawn(r)
                           and release_version(r) and (not latest or
                           version_key(release_version(r)) > version_key(release_version(latest))))
        preview = release_version(candidate) if candidate else None
        plan = {"preview_image": preview or (release_version(latest) if latest else None)}
        if pointer != preview:
            plan["pointer"] = preview
        return plan
    if operation == "Promote":
        if "-" in version:
            raise ValueError("Promote requires a version without a SemVer prerelease part")
        if withdrawn(target):
            raise ValueError("Promote requires a non-withdrawn pre-release")
        if latest and latest["id"] == target["id"] and not target["prerelease"]:
            # The metadata edit succeeded before a failed image move, or
            # the owner accidentally published with pre-release unchecked.
            return {"latest_image": version}
        if not target["prerelease"]:
            raise ValueError("Promote requires a non-withdrawn pre-release")
        if latest and version_key(version) <= version_key(release_version(latest)):
            raise ValueError("Promote requires a version newer than latest")
        return {"promote": target, "latest_image": version}
    if operation != "Withdraw":
        raise ValueError("Unknown channel operation")
    good = [r for r in releases if r["id"] != target["id"] and published(r)
            and not withdrawn(r) and release_version(r)]
    stable = newest(r for r in good if not r["prerelease"])
    is_latest = latest and latest["id"] == target["id"]
    remaining_latest = stable if is_latest else latest
    candidate = newest(good)
    # Preview clients already consider latest; no redundant pointer needed.
    preview = candidate if candidate and (not remaining_latest or
        version_key(release_version(candidate)) > version_key(release_version(remaining_latest))) else None
    plan = {"withdraw": target,
            "latest_image": release_version(remaining_latest) if remaining_latest else None,
            "preview_image": release_version(preview or remaining_latest) if preview or remaining_latest else None}
    if is_latest and stable:
        plan["fallback"] = stable
    if pointer == version:
        plan["pointer"] = release_version(preview) if preview else None
    return plan


def command(args, *, optional=False):
    result = subprocess.run(args, capture_output=True, check=False, timeout=90)
    if result.returncode:
        # Only an actual 404 means absent, never a permission/network failure.
        if optional and b"(HTTP 404)" in result.stderr:
            return None
        error = result.stderr.decode("utf-8", errors="replace").strip()
        raise RuntimeError(f"{args[0]} command failed (exit {result.returncode}): {error}")
    return result.stdout


class GitHub:
    def __init__(self, repository):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
            raise ValueError("Invalid repository")
        self.base = f"repos/{repository}/releases"

    def api(self, endpoint, *, optional=False, binary=False):
        args = ["gh", "api", self.base + endpoint]
        if binary:
            args += ["-H", "Accept: application/octet-stream"]
        data = command(args, optional=optional)
        return data if binary or data is None else json.loads(data)

    def snapshot(self):
        pages = json.loads(command(["gh", "api", "--paginate", "--slurp", self.base + "?per_page=100"]))
        releases = [r for page in pages for r in page]
        latest = self.api("/latest", optional=True)
        if latest and (not release_version(latest) or withdrawn(latest) or latest["prerelease"] or not published(latest)):
            raise ValueError("Latest release is not a usable stable release")
        pointer_release = self.api("/tags/" + POINTER, optional=True)
        pointer = None
        if pointer_release:
            if pointer_release["draft"] or not pointer_release["prerelease"]:
                raise ValueError("Preview pointer must be a published pre-release")
            assets = pointer_release.get("assets", [])
            if len(assets) > 1 or any(a["name"] != "VERSION" for a in assets):
                raise ValueError("Preview pointer may contain only VERSION")
            # Failed GitHub uploads can leave an empty 'starter' asset.
            # Treat it as missing so the next --clobber can replace it.
            if assets and assets[0].get("state", "uploaded") != "starter":
                if assets[0]["size"] > 64:
                    raise ValueError("Preview VERSION exceeds 64 bytes")
                data = self.api(f"/assets/{assets[0]['id']}", binary=True, optional=True)
                # A concurrent/interrupted replacement can remove the asset
                # between metadata and download. Only its 404 is recoverable.
                if data is not None:
                    if len(data) > 64 or not data.endswith(b"\n"):
                        raise ValueError("Preview VERSION must end with a newline and fit in 64 bytes")
                    pointer = data[:-1].decode("ascii")
                    version_key(pointer)
        return releases, latest, pointer

    def edit(self, release, *flags):
        command(["gh", "release", "edit", release["tag_name"], *flags])

    def pointer(self, version):
        current = self.api("/tags/" + POINTER, optional=True)
        if version is None:
            if current and current.get("assets"):
                command(["gh", "release", "delete-asset", POINTER, "VERSION", "--yes"])
            return
        with tempfile.TemporaryDirectory(prefix="release-channel-") as directory:
            path = Path(directory) / "VERSION"
            path.write_bytes((version + "\n").encode("ascii"))
            if current is None:
                command(["gh", "release", "create", POINTER, "--prerelease", "--latest=false",
                         "--target", "main", "--title", "Preview channel", "--notes",
                         "Preview channel pointer. Release manifests and signatures remain on each versioned release."])
            command(["gh", "release", "upload", POINTER, str(path), "--clobber"])


class Images:
    def digest(self, tag, *, optional=False):
        result = subprocess.run(["docker", "buildx", "imagetools", "inspect", f"{IMAGE}:{tag}",
                                 "--format", "{{.Manifest.Digest}}"],
                                capture_output=True, check=False, timeout=90)
        if result.returncode:
            error = result.stderr.lower()
            if optional and not any(s in error for s in (b"unauthorized", b"denied", b"forbidden", b"401", b"403")) \
                    and any(s in error for s in (b"manifest unknown", b"not found")):
                return None
            detail = result.stderr.decode("utf-8", errors="replace").strip()
            raise RuntimeError(f"Cannot inspect image :{tag}: {detail}")
        digest = result.stdout.decode("ascii").strip()
        if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
            raise ValueError("Invalid image digest")
        return digest

    def move(self, tag, digest):
        if digest is None:
            print(f"::warning::No good fallback for {IMAGE}:{tag}; alias left unchanged. "
                  "Manually repoint it when a good image is available, and stop using this alias until then.")
            return
        command(["docker", "buildx", "imagetools", "create", "--prefer-index=false",
                 "--tag", f"{IMAGE}:{tag}", f"{IMAGE}@{digest}"])
        if self.digest(tag) != digest:
            raise RuntimeError("Retag changed the image digest")


def reason_text(reason):
    if not reason.strip() or len(reason.encode("utf-8")) > 4000 or any(
            ord(c) < 32 and c not in "\n\t" or ord(c) == 127 for c in reason):
        raise ValueError("Withdraw reason must be nonempty plain text, at most 4000 bytes, without control characters")
    return reason.strip()


def apply(operation, version, reason, github, images):
    version_key(version)
    if operation == "Withdraw":
        reason = reason_text(reason)
    elif operation not in ("publish", "Promote") or reason:
        raise ValueError("Invalid operation or unexpected reason")
    releases, latest, pointer = github.snapshot()
    plan = decide(operation, version, releases, latest, pointer)
    if not plan:
        print("No channel change: publication of a withdrawn release")
        return
    # Resolve every needed version image before modifying releases or aliases.
    versions = {v for k, v in plan.items() if k.endswith("_image") and v}
    digests = {v: images.digest(v) for v in versions}
    bad_digest = images.digest(version, optional=True) if operation == "Withdraw" else None
    moves = {}
    for key, tag in (("latest_image", "latest"), ("preview_image", "preview")):
        if key in plan:
            current = images.digest(tag, optional=True)
            affected = bad_digest is not None and current == bad_digest
            missing_with_fallback = current is None and plan[key] is not None
            if operation != "Withdraw" or affected or missing_with_fallback or plan[key] is None:
                moves[tag] = digests.get(plan[key])
    if operation == "Promote":
        target = require_release(releases, version)
        age = datetime.now(timezone.utc) - datetime.fromisoformat(target["published_at"].replace("Z", "+00:00"))
        print(f"v{version} has been published for {age}; no minimum soak time")
        if "promote" in plan:
            github.edit(target, "--prerelease=false", "--latest")
        else:
            print("Release is already stable/latest; repairing only the :latest image alias")
    if "fallback" in plan:
        github.edit(plan["fallback"], "--latest")
    if "pointer" in plan:
        github.pointer(plan["pointer"])
    for tag, digest in moves.items():
        images.move(tag, digest)
    if "withdraw" in plan:
        target = plan["withdraw"]
        if withdrawn(target):
            # A retry must preserve the first withdrawal reason and notes.
            github.edit(target, "--prerelease", "--latest=false")
            print("Already withdrawn; original withdrawal notes retained")
            return
        title = target.get("name") or target["tag_name"]
        title += " (withdrawn)"
        # A text code block keeps Markdown/HTML in the reason inert. The
        # delimiter is longer than any backtick run in untrusted plain text.
        fence = "`" * max(3, 1 + max((len(s) for s in re.findall(r"`+", reason)), default=0))
        notes = f"Withdrawn: do not install this release.\n\n{fence}text\n{reason}\n{fence}\n\n" + (target.get("body") or "")
        with tempfile.TemporaryDirectory(prefix="release-withdraw-") as directory:
            path = Path(directory) / "notes.md"
            path.write_text(notes, encoding="utf-8", newline="\n")
            github.edit(target, "--title", title, "--notes-file", str(path), "--prerelease", "--latest=false")
        print("Withdrawn; existing installs stay in place. Fix forward or use open-ferry update -rollback")


def main():
    event = os.environ.get("CHANNEL_EVENT")
    if event == "release":
        tag = os.environ.get("CHANNEL_TAG", "")
        if not tag.startswith("v"):
            print("Ignoring non-version release")
            return
        operation, version, reason = "publish", tag[1:], ""
    elif event == "workflow_dispatch":
        if os.environ.get("GITHUB_REF") != "refs/heads/main":
            raise ValueError("Dispatch channel operations from main")
        operation = os.environ.get("CHANNEL_OPERATION", "")
        version = os.environ.get("CHANNEL_VERSION", "")
        reason = os.environ.get("CHANNEL_REASON", "")
    else:
        raise ValueError("Unsupported event")
    apply(operation, version, reason, GitHub(os.environ["GH_REPO"]), Images())


if __name__ == "__main__":
    try:
        main()
    except (ValueError, RuntimeError, OSError) as error:
        # Keep CLI stderr for diagnosis; reasons never enter CLI arguments.
        raise SystemExit(f"Release channel operation failed: {type(error).__name__}: "
                         + (str(error) if isinstance(error, (ValueError, RuntimeError)) else "I/O failure")) from None
