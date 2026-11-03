#!/usr/bin/env python3
"""Release helpers for ranikust (ADR-0089, ADR-0092). Run with `python3 -I`
(3.11 or later, for tomllib).

Subcommands:

  version                       print [workspace.package] version
  check-tag <tag>               fail unless <tag> is "v" + that version
  previous-tag <tag>            print the release tag before <tag> on its
                                history, or nothing for the first release
  notes <version>               print the CHANGELOG.md section of <version>
  check-environment <owner/repo> <name>
                                fail unless the GitHub environment exists with
                                a required reviewer and deploys only `v*` tags
  notices <cargo-about-output> <out>
                                write THIRD-PARTY-LICENSES: cargo-about's
                                output, then the toolchain's notices from
                                licenses/ (SHA-256 checked)
  package <binary> <third-party-licenses> <version> <outdir>
                                write the release tarball, with the licence
                                notices cargo-about generated, and SHA256SUMS
  prepare <version>             set the workspace version, refresh Cargo.lock
                                and draft the CHANGELOG.md section from the
                                `## Changelog` sections of the pull requests
                                merged since the last tag

`prepare` (`just release-prepare`) only edits files in the working tree: it
never commits, tags or pushes. The release workflow
(`.github/workflows/release.yml`) runs the others. Tests:
tests/release/test_release.py (`just test-release`).
"""

from __future__ import annotations

import datetime
import gzip
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import tarfile
import tomllib
import urllib.parse
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CARGO_TOML = ROOT / "Cargo.toml"
CHANGELOG = ROOT / "CHANGELOG.md"
TARGET = "x86_64-unknown-linux-musl"
# Files shipped next to the binary, relative to the repository root. NOTICE
# carries the attributions the Apache-2.0 licence asks to pass on.
DOCS = ["LICENSE-APACHE", "LICENSE-MIT", "NOTICE", "README.md"]
SEMVER = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z.-]+))?"
)
# The first line of scripts/third-party-licenses.hbs's output.
LICENSES_HEADER = "THIRD-PARTY LICENSES\n"
# The section `notices` appends after cargo-about's output: what the Rust
# toolchain links into the binary, which cargo-about does not see
# (ADR-0094; provenance in licenses/README.md). Each file is checked against
# its SHA-256 before it is shipped.
TOOLCHAIN_HEADER = "Toolchain components\n--------------------\n"
RUST_RELEASE = "1.99.1"
MUSL_RELEASE = "1.2.5"
# rust-lang/llvm-project at the commit Rust 1.99.1's src/llvm-project points
# to: LLVM 23.1.1.
LLVM_COMMIT = "1b9c0d5ff9bbe7634aead059efe6b11a7eeba145"
TOOLCHAIN_FILES = [
    (
        f"Rust {RUST_RELEASE} COPYRIGHT",
        f"https://github.com/rust-lang/rust/blob/{RUST_RELEASE}/COPYRIGHT",
        Path("licenses") / f"rust-{RUST_RELEASE}-COPYRIGHT",
        "172020dbfd5b53a226dfde77616190a48dcff519b0bc0e6deb91a8450782c4af",
    ),
    (
        f"musl libc {MUSL_RELEASE} COPYRIGHT",
        f"https://git.musl-libc.org/cgit/musl/tree/COPYRIGHT?h=v{MUSL_RELEASE}",
        Path("licenses") / f"musl-{MUSL_RELEASE}-COPYRIGHT",
        "f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af",
    ),
]
TOOLCHAIN_INTRO = f"""\
The Rust toolchain ({RUST_RELEASE}) links these into the binary too:

- The Rust standard library and compiler-builtins, used under Apache-2.0,
  whose text is LICENSE-APACHE in this tarball (Rust is offered under
  MIT OR Apache-2.0). Rust {RUST_RELEASE}'s COPYRIGHT follows; the notices
  of the standard library's own third-party code are in
  COPYRIGHT-library.html, shipped with every Rust release in
  share/doc/rust/. compiler-builtins also holds code from LLVM's
  compiler-rt, under Apache-2.0 WITH LLVM-exception (as libunwind below).
- musl libc {MUSL_RELEASE}, as Rust {RUST_RELEASE} builds it for
  x86_64-unknown-linux-musl (with security patches): MIT. Its COPYRIGHT
  follows.
- LLVM's libunwind, from LLVM 23.1.1 as Rust {RUST_RELEASE} uses it
  (rust-lang/llvm-project {LLVM_COMMIT[:7]}): Apache-2.0 WITH
  LLVM-exception, whose exception waives the notice conditions for code
  embedded in a binary, so its text is not reproduced:
  https://github.com/rust-lang/llvm-project/blob/{LLVM_COMMIT}/libunwind/LICENSE.TXT
"""
# Keep a Changelog sections, in CHANGELOG.md's order.
SECTIONS = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]
# More merged pull requests than this since one release and `prepare` stops
# rather than draft from a truncated list.
PR_LIMIT = 1000

# The verification commands name the release workflow and the tag: without
# them `gh attestation verify --repo` accepts an attestation made by any
# workflow of the repository on any ref (ADR-0092). gh before 2.102.0 matched
# --signer-workflow by prefix and --source-ref without case.
INSTALL = """\
ranikust {version}: static musl binary for x86_64 Linux (Linux 5.8 or later)

You are reading this inside the tarball. If you have not checked the
tarball yet, do it now, in the directory holding it and the release's
SHA256SUMS, with GitHub CLI 2.102.0 or later:

    sha256sum -c SHA256SUMS --ignore-missing
    gh attestation verify ranikust-{version}-{target}.tar.gz \\
      --repo {repo} \\
      --signer-workflow {repo}/.github/workflows/release.yml \\
      --source-ref refs/tags/v{version} \\
      --deny-self-hosted-runners

The second command checks that the tarball was built from the tag v{version}
by this repository's release workflow on a GitHub-hosted runner (build
provenance). If either check fails, delete the files and do not run them.

THIRD-PARTY-LICENSES holds the licence notices of what the binary
contains: the crates, the C code they build, and the Rust toolchain's
standard library and musl libc.

Install:

    install -m 0755 ranikust-{version}-{target}/ranikust /usr/local/bin/ranikust
    ranikust version

ranikust is pre-alpha. The release notes say what works and what does not:
https://github.com/{repo}/releases/tag/v{version}
The guide: https://github.com/{repo}/tree/v{version}/docs/book/src
"""


def die(message: str, code: int = 1) -> None:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(code)


def run(args: list[str], *, check: bool = True) -> str:
    result = subprocess.run(
        args, cwd=ROOT, check=False, capture_output=True, text=True
    )
    if check and result.returncode != 0:
        die(f"{' '.join(args)} failed: {result.stderr.strip()}")
    return result.stdout


def workspace_package() -> dict:
    try:
        with CARGO_TOML.open("rb") as f:
            package = tomllib.load(f)["workspace"]["package"]
    except (OSError, tomllib.TOMLDecodeError, KeyError) as error:
        die(f"cannot read [workspace.package] from {CARGO_TOML}: {error!r}")
    return package


def workspace_version() -> str:
    version = workspace_package().get("version")
    if not isinstance(version, str):
        die("Cargo.toml's [workspace.package] has no version")
    return version


def repository() -> str:
    """`owner/name` from [workspace.package] repository."""
    url = workspace_package().get("repository")
    if not isinstance(url, str):
        die("Cargo.toml's [workspace.package] has no repository")
    match = re.fullmatch(r"https://github\.com/([^/]+/[^/]+?)/?", url)
    if not match:
        die(f"[workspace.package] repository is not a GitHub URL: {url}")
    return match.group(1)


def check_version(version: str) -> None:
    if not SEMVER.fullmatch(version):
        die(f"{version!r} is not a version such as 0.1.0 or 0.1.0-rc.1 (no 'v')")


def version_key(version: str) -> tuple:
    """A sort key in Semantic Versioning precedence: a pre-release sorts
    before its release, numeric identifiers numerically and before
    alphanumeric ones, a longer list of identifiers after its prefix."""
    match = SEMVER.fullmatch(version)
    if not match:
        die(f"{version!r} is not a version")
    major, minor, patch, pre = match.groups()
    if pre is None:
        pre_key: tuple = (1,)
    else:
        pre_key = (0, tuple(
            (0, int(part), "") if part.isascii() and part.isdigit() else (1, 0, part)
            for part in pre.split(".")
        ))
    return (int(major), int(minor), int(patch), pre_key)


def is_newer(new: str, old: str) -> bool:
    return version_key(new) > version_key(old)


# ---------------------------------------------------------------- tags


def describe(args: list[str]) -> str | None:
    """`git describe --tags --abbrev=0 --match v[0-9]* <args>`: the tag, or
    None when no release tag describes the commit; any other failure (a bad
    ref, no repository) dies."""
    # LC_ALL=C: the messages below are matched in English, whatever the
    # locale git would translate them into.
    result = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v[0-9]*", *args],
        cwd=ROOT, check=False, capture_output=True, text=True,
        env={**os.environ, "LC_ALL": "C"},
    )
    if result.returncode == 0:
        return result.stdout.strip() or None
    if "No names found" in result.stderr or "No tags can describe" in result.stderr:
        return None
    die(f"git describe {' '.join(args)} failed: {result.stderr.strip()}")
    return None  # unreachable


def previous_tag(tag: str) -> str | None:
    """The latest `v*` tag on the history of <tag>, <tag> itself excluded;
    None for the first release."""
    return describe(["--exclude", tag, f"{tag}^{{commit}}"])


def remote_tag_exists(tag: str) -> bool:
    out = run(["git", "ls-remote", "--tags", "origin", f"refs/tags/{tag}"])
    return bool(out.strip())


# ---------------------------------------------------------------- notes


def changelog_section(text: str, version: str) -> str | None:
    """The body of `## [version]` in CHANGELOG.md, without its heading."""
    heading = re.compile(
        r"## \[" + re.escape(version) + r"\](?: - [0-9]{4}-[0-9]{2}-[0-9]{2})?[ \t]*"
    )
    lines = text.splitlines()
    for i, line in enumerate(lines):
        if heading.fullmatch(line):
            body = []
            for rest in lines[i + 1 :]:
                if rest.startswith("## [") or re.match(r"^\[[^\]]+\]: ", rest):
                    break
                body.append(rest)
            return "\n".join(body).strip("\n")
    return None


MARKDOWN_BLOCK = re.compile(r"(#{1,6} |[-*+] |\d+[.)] |>|\||<!--|```|~~~)")


def unwrap_markdown(text: str) -> str:
    """Join hard-wrapped lines into one line per paragraph or list item.

    CHANGELOG.md wraps prose at about 80 columns, but GitHub renders every
    newline of a release body as a line break. A line joins the one before it
    unless it is blank or starts a block (heading, list item, quote, table,
    comment, code fence); headings, tables and comments take no continuation,
    and code fences are copied as they are. Words and their order are kept.
    """
    out: list[str] = []
    in_fence = False
    joinable = False
    for line in text.split("\n"):
        stripped = line.strip()
        if stripped.startswith(("```", "~~~")):
            in_fence = not in_fence
            out.append(line)
            joinable = False
        elif in_fence or not stripped:
            out.append(line)
            joinable = False
        elif joinable and not MARKDOWN_BLOCK.match(stripped):
            out[-1] = out[-1].rstrip() + " " + stripped
        else:
            out.append(line.rstrip())
            joinable = not stripped.startswith(("#", "|", "<!--", ">"))
    return "\n".join(out)


def cmd_notes(version: str) -> None:
    check_version(version)
    body = changelog_section(CHANGELOG.read_text(encoding="utf-8"), version)
    if body is None:
        die(f"CHANGELOG.md has no '## [{version}]' section")
    if not re.sub(r"<!--.*?-->", "", body, flags=re.S).strip():
        die(f"CHANGELOG.md's '## [{version}]' section is empty")
    if "<!-- Draft" in body:
        die(
            f"CHANGELOG.md's '## [{version}]' section still holds the draft "
            "marker of `just release-prepare`: edit it and remove the marker"
        )
    print(unwrap_markdown(body))


# ---------------------------------------------------------------- environment


def environment_problems(env: dict, policies: dict) -> list[str]:
    """What keeps a GitHub environment from guarding a release: it must name
    a required reviewer and let only `v*` tags deploy (ADR-0092)."""
    problems = []
    reviewers = [
        reviewer
        for rule in env.get("protection_rules") or []
        if rule.get("type") == "required_reviewers"
        for reviewer in rule.get("reviewers") or []
    ]
    if not reviewers:
        problems.append("it has no required reviewer, so nobody approves a release")
    if any(
        rule.get("prevent_self_review")
        for rule in env.get("protection_rules") or []
        if rule.get("type") == "required_reviewers"
    ):
        problems.append(
            "'Prevent self-reviews' is on: the maintainer, who pushes the tag, "
            "could not approve its release, and there is no other reviewer"
        )
    policy = env.get("deployment_branch_policy") or {}
    if not policy.get("custom_branch_policies") or policy.get("protected_branches"):
        problems.append(
            "its deployments are not limited to selected tags "
            "(set 'Selected branches and tags')"
        )
    items = (policies or {}).get("branch_policies") or []
    if not items:
        problems.append("it has no deployment tag rule 'v*'")
    for item in items:
        if item.get("type") != "tag" or item.get("name") != "v*":
            problems.append(
                f"its deployment rule {item.get('type')} {item.get('name')!r} "
                "is not the tag rule 'v*'"
            )
    return problems


def gh_api(path: str, missing: str) -> dict:
    result = subprocess.run(
        ["gh", "api", path], cwd=ROOT, check=False, capture_output=True, text=True
    )
    if result.returncode != 0:
        error = (result.stderr or result.stdout).strip()
        if "HTTP 404" in error:
            die(missing)
        die(f"gh api {path} failed: {error}")
    return json.loads(result.stdout)


def cmd_check_environment(repo: str, name: str) -> None:
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
        die(f"{repo!r} is not owner/name")
    base = f"repos/{repo}/environments/{urllib.parse.quote(name, safe='')}"
    setup = "(CONTRIBUTING.md, Releasing: the maintainer sets it up before the first tag)"
    env = gh_api(
        base,
        f"the environment {name!r} does not exist; a job naming it would create "
        f"it without protection {setup}",
    )
    policies = gh_api(
        f"{base}/deployment-branch-policies",
        f"the environment {name!r} has no deployment tag rules {setup}",
    )
    problems = environment_problems(env, policies)
    if problems:
        die(f"the environment {name!r} does not guard a release: " + "; ".join(problems) + f" {setup}")
    print(f"environment {name!r}: required reviewer, deploys only v* tags")


# ---------------------------------------------------------------- package


def source_date_epoch() -> int:
    """SOURCE_DATE_EPOCH, else the commit time of HEAD: the tarball's bytes
    then depend only on its inputs."""
    value = os.environ.get("SOURCE_DATE_EPOCH")
    if value is not None:
        if not re.fullmatch(r"[0-9]+", value):
            die(f"SOURCE_DATE_EPOCH is not a whole number of seconds: {value!r}")
        return int(value)
    return int(run(["git", "log", "-1", "--format=%ct"]).strip())


def add_file(tar: tarfile.TarFile, name: str, data: bytes, mode: int, mtime: int) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    info.mtime = mtime
    info.uid = info.gid = 0
    info.uname = info.gname = ""
    tar.addfile(info, io.BytesIO(data))


SEPARATOR = "\n" + "=" * 80 + "\n"


def check_generated(path: str, text: str) -> None:
    """cargo-about's output through scripts/third-party-licenses.hbs."""
    if not text.startswith(LICENSES_HEADER) or "\nLicence texts\n" not in text:
        die(f"{path} is not the output of scripts/third-party-licenses.hbs")
    if SEPARATOR not in text:
        die(f"{path} lists no licence text")


def toolchain_section() -> str:
    """The licence notices of the toolchain's components, from the committed
    files, each checked against its SHA-256."""
    parts = [TOOLCHAIN_HEADER, TOOLCHAIN_INTRO]
    for title, url, path, sha256 in TOOLCHAIN_FILES:
        try:
            data = (ROOT / path).read_bytes()
        except OSError as error:
            die(f"cannot read {path}: {error}")
        if hashlib.sha256(data).hexdigest() != sha256:
            die(f"{path} does not have the SHA-256 recorded in scripts/release.py "
                "(licenses/README.md says where it comes from)")
        parts.append(
            SEPARATOR.lstrip("\n") + f"{title}\n{url}\n" + "-" * 80 + "\n\n"
            + data.decode("utf-8").rstrip("\n") + "\n"
        )
    return "\n".join(parts)


def notices_text(generated: str) -> str:
    """THIRD-PARTY-LICENSES: cargo-about's output, then the toolchain's."""
    if TOOLCHAIN_HEADER in generated:
        die("the generated file already has a toolchain section")
    return generated.rstrip("\n") + "\n\n\n" + toolchain_section()


def cmd_notices(generated: str, out: str) -> None:
    try:
        text = Path(generated).read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        die(f"cannot read {generated}: {error}")
    check_generated(generated, text)
    Path(out).write_text(notices_text(text), encoding="utf-8")
    print(out)


def read_third_party_licenses(path: str) -> bytes:
    """THIRD-PARTY-LICENSES as `notices` wrote it (ADR-0094): a release
    without the notices of the code it contains is not packaged."""
    file = Path(path)
    if not file.is_file():
        die(f"{path} is not a file: generate it with `just third-party-licenses`")
    data = file.read_bytes()
    text = data.decode("utf-8", errors="replace")
    check_generated(path, text)
    if not text.endswith(toolchain_section()):
        die(f"{path} does not end with the toolchain section `notices` adds")
    return data


def cmd_package(binary: str, licenses: str, version: str, outdir: str) -> None:
    check_version(version)
    bin_path = Path(binary)
    if not bin_path.is_file():
        die(f"{binary} is not a file")
    third_party = read_third_party_licenses(licenses)
    out = Path(outdir)
    out.mkdir(parents=True, exist_ok=True)
    stem = f"ranikust-{version}-{TARGET}"
    tarball = out / f"{stem}.tar.gz"
    epoch = source_date_epoch()
    repo = repository()

    entries: list[tuple[str, bytes, int]] = [
        ("ranikust", bin_path.read_bytes(), 0o755),
        (
            "INSTALL",
            INSTALL.format(version=version, target=TARGET, repo=repo).encode(),
            0o644,
        ),
        ("THIRD-PARTY-LICENSES", third_party, 0o644),
    ]
    for doc in DOCS:
        entries.append((doc, (ROOT / doc).read_bytes(), 0o644))
    entries.sort(key=lambda entry: entry[0])

    # Deterministic: sorted names, fixed owner, mode and time, no name or
    # time in the gzip header.
    with tarball.open("wb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as gz:
            with tarfile.open(fileobj=gz, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                directory = tarfile.TarInfo(stem)
                directory.type = tarfile.DIRTYPE
                directory.mode = 0o755
                directory.mtime = epoch
                directory.uid = directory.gid = 0
                directory.uname = directory.gname = ""
                tar.addfile(directory)
                for name, data, mode in entries:
                    add_file(tar, f"{stem}/{name}", data, mode, epoch)

    digest = hashlib.sha256(tarball.read_bytes()).hexdigest()
    # The format `sha256sum -c` reads: digest, two spaces, file name.
    (out / "SHA256SUMS").write_text(f"{digest}  {tarball.name}\n", encoding="utf-8")
    print(tarball)
    print(out / "SHA256SUMS")


# ---------------------------------------------------------------- prepare


def set_workspace_version(text: str, version: str) -> str:
    match = re.search(r"(?ms)^\[workspace\.package\]\n(.*?)(?=^\[|\Z)", text)
    if not match:
        die("Cargo.toml has no [workspace.package] table")
    table = match.group(1)
    new_table, count = re.subn(
        r'(?m)^version = "[^"]*"$', f'version = "{version}"', table, count=1
    )
    if count != 1:
        die("Cargo.toml's [workspace.package] has no `version = \"...\"` line")
    return text[: match.start(1)] + new_table + text[match.end(1) :]


def last_tag() -> str | None:
    """The latest `v*` tag on HEAD's history; None before the first release."""
    return describe([])


def changelog_text(body: str | None) -> str | None:
    """The `## Changelog` section of a pull request body, as the pull
    request template asks for it (ADR-0088): bullets under Keep a Changelog
    `###` headings or alone, or "none". None when the section is absent,
    holds only the template's comment, or says "none" (bare, quoted, in
    backticks or emphasis, with or without a full stop)."""
    body = re.sub(r"<!--.*?-->", "", body or "", flags=re.S).replace("\r\n", "\n")
    match = re.search(r"(?ms)^##[ \t]+Changelog[ \t]*\n(.*?)(?=^##[ \t]|\Z)", body)
    if not match:
        return None
    text = match.group(1).strip()
    bare = text.strip("\"'`*_ ").rstrip(".").strip("\"'`*_ ")
    if not bare or re.fullmatch(r"(?i)none|n/a|no|-|nothing", bare):
        return None
    return text


def section_for_title(title: str, labels: set[str]) -> str:
    """The Keep a Changelog section of a pull request without `###`
    sub-headings in its entry, by the rules of cliff.toml."""
    if "security" in labels or re.match(r"^[a-z]+\(security\)", title):
        return "Security"
    if re.match(r"^[a-z]+(\([^)]*\))?!?: [Ss]ecurity", title):
        return "Security"
    if re.match(r"^[a-z]+(\([^)]*\))?!?: [Dd]eprecate", title):
        return "Deprecated"
    if re.match(r"^[a-z]+(\([^)]*\))?!?: [Rr]emove", title):
        return "Removed"
    if title.startswith("feat"):
        return "Added"
    if title.startswith("fix"):
        return "Fixed"
    # perf, revert, breaking changes and anything else its author wrote an
    # entry for.
    return "Changed"


def entries_of(text: str) -> list[tuple[str | None, list[str]]]:
    """Split an entry into (sub-heading or None, bullet blocks)."""
    groups: list[tuple[str | None, list[str]]] = []
    heading: str | None = None
    lines: list[str] = []

    def flush() -> None:
        # A top-level bullet starts a block; other non-blank lines continue
        # the current one; text before any bullet becomes a bullet itself.
        blocks: list[str] = []
        for line in lines:
            if not line.strip():
                continue
            bullet = re.match(r"^[-*] (.*)$", line)
            if bullet:
                blocks.append("- " + bullet.group(1))
            elif not blocks:
                blocks.append("- " + line.strip())
            else:
                blocks[-1] += "\n" + line
        if blocks:
            groups.append((heading, blocks))

    for line in text.splitlines():
        sub = re.match(r"^#{3,4}[ \t]+(.+?)[ \t:]*$", line)
        if sub:
            flush()
            lines = []
            name = sub.group(1).strip().capitalize()
            heading = name if name in SECTIONS else None
            continue
        lines.append(line)
    flush()
    return groups


def merged_pull_requests(repo: str, since: str | None) -> list[dict]:
    revs = run(["git", "rev-list", f"{since}..HEAD" if since else "HEAD"])
    commits = set(revs.split())
    # Only the pull requests merged since about the last tag's commit are
    # asked for, so the cap applies to one release's worth of them. GitHub
    # reads `merged:>=DAY` in UTC, and the commit's own date is local, so the
    # query starts a day before the commit's UTC day; the earlier pull
    # requests it brings are dropped by the commit set above.
    search = []
    if since:
        stamp = run(["git", "log", "-1", "--format=%ct", f"{since}^{{commit}}"]).strip()
        if not re.fullmatch(r"[0-9]+", stamp):
            die(f"cannot read the time of {since}: {stamp!r}")
        utc_day = datetime.datetime.fromtimestamp(int(stamp), datetime.timezone.utc).date()
        day = utc_day - datetime.timedelta(days=1)
        search = ["--search", f"merged:>={day.isoformat()}"]
    result = subprocess.run(
        [
            "gh", "pr", "list", "--repo", repo, "--state", "merged",
            "--base", "main", "--limit", str(PR_LIMIT), *search,
            "--json", "number,title,body,labels,mergeCommit,url",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        # A draft without the pull requests would look like a release with
        # nothing to say: stop instead.
        die(
            "gh pr list failed, so the merged pull requests are unknown: "
            f"{(result.stderr or result.stdout).strip()}"
        )
    listed = json.loads(result.stdout)
    if len(listed) >= PR_LIMIT:
        die(
            f"gh listed {PR_LIMIT} pull requests merged since "
            f"{since or 'the first commit'}, its limit, so some may be missing: "
            "raise PR_LIMIT"
        )
    prs = [pr for pr in listed if (pr.get("mergeCommit") or {}).get("oid") in commits]
    return sorted(prs, key=lambda pr: pr["number"])


def draft_section(prs: list[dict]) -> tuple[str, list[dict]]:
    by_section: dict[str, list[str]] = {name: [] for name in SECTIONS}
    without: list[dict] = []
    for pr in prs:
        text = changelog_text(pr.get("body"))
        if text is None:
            without.append(pr)
            continue
        labels = {label["name"] for label in pr.get("labels") or []}
        link = f" ([#{pr['number']}]({pr['url']}))"
        for heading, blocks in entries_of(text):
            section = heading or section_for_title(pr["title"], labels)
            by_section[section].extend(block + link for block in blocks)
    parts = []
    for name in SECTIONS:
        if by_section[name]:
            parts.append(f"### {name}\n\n" + "\n".join(by_section[name]))
    return "\n\n".join(parts), without


def insert_section(text: str, version: str, previous: str | None, body: str, repo: str) -> str:
    lines = text.split("\n")
    try:
        start = lines.index("## [Unreleased]")
    except ValueError:
        die("CHANGELOG.md has no '## [Unreleased]' heading")
    end = start + 1
    while end < len(lines) and not (
        lines[end].startswith("## [") or re.match(r"^\[[^\]]+\]: ", lines[end])
    ):
        end += 1
    unreleased = "\n".join(lines[start + 1 : end]).strip("\n")
    today = datetime.datetime.now(datetime.timezone.utc).date().isoformat()

    section = [
        f"## [{version}] - {today}",
        "",
        "<!-- Draft by `just release-prepare`: edit it for users, then delete",
        "     this comment; the release workflow refuses a section that still",
        "     holds it. -->",
    ]
    if body:
        section += ["", body]
    if unreleased:
        section += ["", "<!-- Moved from Unreleased: -->", "", unreleased]
    new = lines[: start + 1] + [""] + section + [""] + lines[end:]

    # Link references: Unreleased compares from the new tag; the new
    # version compares from the previous one.
    out = []
    for line in new:
        if line.startswith("[Unreleased]: "):
            out.append(f"[Unreleased]: https://github.com/{repo}/compare/v{version}...HEAD")
            if previous:
                out.append(f"[{version}]: https://github.com/{repo}/compare/{previous}...v{version}")
            else:
                out.append(f"[{version}]: https://github.com/{repo}/releases/tag/v{version}")
        else:
            out.append(line)
    return "\n".join(out)


def cmd_prepare(version: str) -> None:
    check_version(version)
    current = workspace_version()
    if not is_newer(version, current):
        die(f"{version} is not newer than the workspace version {current}")
    dirty = run(["git", "status", "--porcelain", "--", "Cargo.toml", "Cargo.lock", "CHANGELOG.md"])
    if dirty.strip():
        die("Cargo.toml, Cargo.lock or CHANGELOG.md has uncommitted changes:\n" + dirty)
    tag = f"v{version}"
    if run(["git", "tag", "--list", tag]).strip():
        die(f"the tag {tag} exists already")
    if remote_tag_exists(tag):
        die(f"the tag {tag} exists already on origin")

    # Everything is computed before the first write, so a failure leaves
    # the tree as it was.
    repo = repository()
    previous = last_tag()
    prs = merged_pull_requests(repo, previous)
    body, without = draft_section(prs)
    old_toml = CARGO_TOML.read_text(encoding="utf-8")
    new_toml = set_workspace_version(old_toml, version)
    old_changelog = CHANGELOG.read_text(encoding="utf-8")
    new_changelog = insert_section(old_changelog, version, previous, body, repo)
    lock = ROOT / "Cargo.lock"
    old_lock = lock.read_bytes() if lock.exists() else None

    CARGO_TOML.write_text(new_toml, encoding="utf-8")
    # Only the workspace's own entries change, so no network is needed.
    result = subprocess.run(
        ["cargo", "update", "--workspace", "--offline"],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        CARGO_TOML.write_text(old_toml, encoding="utf-8")
        if old_lock is not None:
            lock.write_bytes(old_lock)
        die(
            "cargo update --workspace --offline failed; Cargo.toml and "
            f"Cargo.lock are back as they were:\n{result.stderr.strip()}"
        )
    CHANGELOG.write_text(new_changelog, encoding="utf-8")

    print(f"version: {current} -> {version}; Cargo.toml, Cargo.lock and CHANGELOG.md edited")
    print(f"pull requests merged since {previous or 'the first commit'}: {len(prs)}")
    print(f"  with a changelog entry: {len(prs) - len(without)}")
    if without:
        print("  without one (check that none of them is user-visible):")
        for pr in without:
            print(f"    #{pr['number']} {pr['title']}")
    print(
        f"next: edit the '## [{version}]' section of CHANGELOG.md, commit it as "
        f"'chore(release): {tag}' on a branch release/{tag} and open "
        "the release pull request; after it is merged, the maintainer pushes "
        f"the tag {tag} (CONTRIBUTING.md, Releasing)"
    )


# ---------------------------------------------------------------- main


def main(argv: list[str]) -> None:
    usage = __doc__ or ""
    if not argv:
        die("missing subcommand\n" + usage, 2)
    command, args = argv[0], argv[1:]
    if command == "version" and not args:
        print(workspace_version())
    elif command == "check-tag" and len(args) == 1:
        expected = "v" + workspace_version()
        if args[0] != expected:
            die(f"the tag {args[0]} does not match the workspace version ({expected})")
        print(f"tag {args[0]} matches the workspace version")
    elif command == "previous-tag" and len(args) == 1:
        print(previous_tag(args[0]) or "")
    elif command == "notes" and len(args) == 1:
        cmd_notes(args[0])
    elif command == "check-environment" and len(args) == 2:
        cmd_check_environment(*args)
    elif command == "notices" and len(args) == 2:
        cmd_notices(*args)
    elif command == "package" and len(args) == 4:
        cmd_package(*args)
    elif command == "prepare" and len(args) == 1:
        cmd_prepare(args[0])
    else:
        die("bad arguments\n" + usage, 2)


if __name__ == "__main__":
    main(sys.argv[1:])
