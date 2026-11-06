---
status: accepted
date: 2026-11-06
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0119: Git contexts take BuildKit's fragment only, with no backend fallback

## Context and Problem Statement

[ADR-0035](0035-git-contexts-built-in-or-external-git.md) clones git contexts
with a built-in `gix` by default and an external `git` on request, and left
three points open: whether a context falls back between the two backends (#15,
Q-250), whether kaniko's `#ref#commit` fragment can name a subdirectory (#16,
Q-251), and which `--git-config` keys the built-in honours (#17, Q-252). It also
says `ADD <git>` uses "the same backend and options", and the options include
kaniko's `GIT_USERNAME`, `GIT_PASSWORD` and `GIT_TOKEN`. A context URL comes
from the operator; an `ADD` URL comes from the Dockerfile, which may name any
host. BuildKit sends only the secrets the operator passes. So a fourth question
came up while planning Phase 5 (see [the roadmap](../../README.md#roadmap)):
which credentials may a fetch the Dockerfile names send?

## Considered Options

Fallback (#15):

- (a) None: the built-in fails with an error naming `--git-backend=external`.
- (b) Fall back to a `git` found in `PATH`, with a warning: works more often,
  but runs a host binary and reads its configuration unasked.

Fragment (#16; (d) reverses that part of ADR-0035):

- (a) Refuse a `:subdir` in kaniko's form.
- (b) Accept `#ref:subdir#commit`: a third grammar.
- (c) Subdirectories only through `--context-sub-path`.
- (d) Drop kaniko's `#ref#commit`; accept BuildKit's `#ref:subdir` only, pinning
  by a full commit hash as the ref. kaniko users with `#branch#commit` get an
  error that shows the new form.

`--git-config` keys of the built-in (#17):

- (a) A documented allow-list (proxy, CA file, extra headers, `url.*.insteadOf`,
  `core.sshCommand`), the others refused with a hint to
  `--git-backend=external`.
- (b) Pass every key and let `gix` ignore what it does not know: a key may
  silently do nothing.

Credentials a Dockerfile-named fetch sends:

- (a) None: `ADD` fetches anonymously.
- (b) BuildKit's secrets only, given with `--secret`; kaniko's `GIT_*` variables
  never go to an `ADD` host.
- (c) Also the `GIT_*` variables, as "same options" can be read: they would go
  to any host a Dockerfile names.

## Decision Outcome

A git context is cloned by the backend the operator chose and never by the
other, its fragment is BuildKit's `#ref:subdir` only, the built-in backend
honours an allow-list of `--git-config` keys, and a fetch a Dockerfile names
sends only the secrets the operator passes with `--secret`, as BuildKit does.

The maintainer's answer (2026-11-06: "Một cú pháp BuildKit (khuyến nghị)", "One
BuildKit syntax (recommended)") stood for option (a) of #15, option (d) of #16,
option (a) of #17 and option (b) of the credentials question.

- **No fallback (#15 (a)).** A feature the built-in lacks is an error that names
  `--git-backend=external`; ranikust never runs the host's `git`, or reads its
  configuration, unless the operator chose it.
- **One fragment (#16 (d)).** The fragment is `#ref:subdir`. kaniko's
  `#ref#commit` is dropped: a full commit hash as the ref pins the source as
  kaniko's second part did. A `#branch#commit` URL is an error that shows the
  new form. `--git KEY=VALUE` (kaniko's keys) is not a fragment and is
  unchanged.
- **An allow-list (#17 (a)).** The built-in honours documented keys: proxy, CA
  file, extra headers, `url.*.insteadOf` and `core.sshCommand`. Any other key is
  refused with a hint to `--git-backend=external`, so a key that does nothing
  cannot surprise.
- **`ADD` credentials (b).** An `ADD <url>` or `ADD <git>` fetch sends only
  BuildKit's secrets, which the operator passes with `--secret`:
  `HTTP_AUTH_HEADER_<host>`, `HTTP_AUTH_TOKEN_<host>`, `GIT_AUTH_TOKEN[.<host>]`
  and `GIT_AUTH_HEADER[.<host>]`. The operator opts in per secret. The
  host-suffixed names go only to their host; the unsuffixed `GIT_AUTH_TOKEN` and
  `GIT_AUTH_HEADER` go to every `ADD <git>` host, as in BuildKit. ADR-0035's
  "same backend and options" for `ADD <git>` is read as not covering
  credentials: kaniko's `GIT_USERNAME`, `GIT_PASSWORD` and `GIT_TOKEN` never go
  to an `ADD` host, and still apply to a git context, whose URL the operator
  names.

ADR-0035 is partly superseded: its two fragment forms, and its three open points
(fallback between backends, Q-250 (#15); the fragment, Q-251 (#16); the
`--git-config` keys, Q-252 (#17)), are replaced by the above. Its backends,
flags, hardening and the rest stand. Not covered: the `?ref=…&subdir=…` query
form that ADR-0035 lists beside the fragment, which this decision does not
touch.

### Consequences

- Good, because one fragment grammar means no URL is read two ways.
- Good, because the host's `git` and its configuration are never read unasked.
- Good, because a key that silently does nothing is refused instead.
- Good, because a Dockerfile cannot send the operator's `GIT_*` credentials to a
  host it names (threat model T11).
- Bad, because a kaniko pipeline with `#branch#commit` fails until its URL
  changes; the error shows the new form.
- Bad, because a build that needs a protocol or option the built-in lacks fails
  and must choose `--git-backend=external`.
- Bad, because an `ADD <git>` that needs authentication needs a `--secret`:
  kaniko's `GIT_*` variables do not work there.
- Bad, because an unsuffixed `GIT_AUTH_TOKEN` or `GIT_AUTH_HEADER` goes to every
  host an `ADD <git>` names, as in BuildKit; only the host-suffixed names are
  limited to one host.

## Evidence

- **[external]** kaniko@44aa7ddc0: the fragment is `#ref#commit` with no
  subdirectory (`pkg/buildcontext/git.go:58`, `:126-128`); its credentials are
  the `GIT_USERNAME`, `GIT_PASSWORD` and `GIT_TOKEN` variables (`git.go:37-45`,
  `:197-220`). [#137](https://github.com/tran4774/ranikust/issues/137) reads
  kaniko's git integration tests as passing a single fragment part, which still
  works: **[inferred]**, not re-checked.
- **[external]** buildkit@cd8ee244: the fragment is `#ref:subdir`
  (`util/gitutil/git_url.go:71-79`); an `ADD` fetch authenticates only through
  the secrets `HTTP_AUTH_HEADER_<host>`, `HTTP_AUTH_TOKEN_<host>`,
  `GIT_AUTH_TOKEN` and `GIT_AUTH_HEADER`, host-suffixed first
  (`source/http/source.go:243-272`, `source/git/source.go:438-532`); an
  `ADD <url>` fetch reads no netrc.
- ADR-0035 is the only record of the backends, flags and fragments, none built:
  the planned design is in [rnk-context](../agents/crates/rnk-context.md).

## More Information

Answers #15, #16 and #17, and a Phase 5 question on credentials for fetches a
Dockerfile names. **Supersedes ADR-0035 in part** (its fragment forms, and the
three open points it left, Q-250 to Q-252); ADR-0035 stays `accepted` with its
body and front matter unchanged, since only a part of it is replaced. Related:
[ADR-0011](0011-accept-common-kaniko-flags.md),
[ADR-0032](0032-observable-behaviour-follows-buildkit.md),
[ADR-0049](0049-security-and-performance-over-kaniko-compatibility.md),
[ADR-0118](0118-fetches-are-bounded-by-fetch-idle-and-http-contexts-and-fetches-under-network-none-are-refused.md)
(the bounds of a clone).

Follow-ups, with Phase 5's tasks and not before they are built: the
[rnk-context](../agents/crates/rnk-context.md) card's git plan, the book's usage
page, the kaniko map, the threat model (T11, git contexts), and the
`## Changelog` entry.
