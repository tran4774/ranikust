---
status: accepted
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0076: The build context's owner is a principal of its own, trusted less than the operator

## Context and Problem Statement

The threat model (§3) trusted every path the operator types: `--context`,
`--dockerfile` and the outputs (`--digest-file`, `--image-name-with-digest-file`,
`--image-name-tag-with-digest-file`, `--tar-path`, `--oci-layout-path`,
`--metrics-file`). It distrusted only the context's *content*. But a path
the operator types can run through objects someone else controls. A root CI
runner builds a merge request's checkout, and a build service builds a
third-party repository. On a shared host, root builds a directory an
unprivileged account owns.

Whoever commits to that checkout or owns that directory controls every
name in it, symlinks and hardlinks included. ranikust resolved these paths
on the host as root, by name, so that principal could steer root's reads
and writes:

- A committed `digest -> /etc/...` made root truncate a host file.
- A committed `out -> /some/dir` sent the tar, the OCI layout and the
  metrics report there.
- `--dockerfile=$CI_PROJECT_DIR/Dockerfile` (kaniko's GitLab form), and the
  default `Dockerfile` run from the checkout, were read on the host through
  a committed symlink, and the target's first token was printed in the
  parse error.
- `std::fs::canonicalize` of `--context` followed a committed
  `services/api -> /root/.docker`, so `COPY .` copied root's registry
  credentials into the image.
- `COPY` read context files as root, so a root-only file hardlinked into
  an account's context (`fs.protected_hardlinks=0`) entered the image.

BuildKit does not have the problem by construction: the client reads the
context with the invoking user's authority and sends it over the session.
kaniko resolves every one of these paths by name, as ranikust did.

The question: is the context's owner a principal distinct from the
operator, and if so, which rules keep root's reads and writes out of their
reach?

## Considered Options

- **A. The context's owner is a principal of its own, trusted less than
  the operator.** ranikust never lets a name the context's owner controls
  steer one of its own host reads or writes.
- **B. Keep trusting operator paths.** Document instead that an operator
  must not point an output, the Dockerfile or the context at, or through, a
  directory someone less trusted controls.

B leaves the most common CI form, a Dockerfile and outputs in the checkout,
exploitable by a merge request. The maintainer ranks security before
compatibility.

## Decision Outcome

Chosen option: **A**, decided by the maintainer on 2026-11-02 ("1: chốt A").
Threat-model §3 lists the build context's owner as a principal of its own.
Four rules follow, each enforced through file descriptors in `rnk-rootfs`,
with no new flag.

1. **Outputs never follow a symlink and never lead into the context.**
   Every output is written in an `rnk_rootfs::OutputDir`.
   - Its directory is reached from `/` one name at a time. A symlink
     anywhere on the way is refused, with a hint to pass the `realpath`
     form.
   - Missing directories are made by `mkdirat` below the last one reached,
     then opened again without following a symlink.
   - A path that reaches the context's directory is refused, even one that
     leaves it again with `..`. The context is told by its inode, so a bind
     mount of it counts too.
   - The digest files are still written in place, as kaniko writes them, so
     a file mounted there (a Kubernetes termination log) keeps its inode.
     The existing entry is pinned first, and must be a regular file with one
     link: a symlink, a hardlink, a FIFO or a device is refused, never
     opened.
   - The other outputs replace their name through an exclusive temporary
     file renamed into place.
   - The paths are checked before the build (`--metrics-file` before its
     report is set up, so nothing is written there) and again as they are
     written.
2. **A Dockerfile path is read through the context once it enters it.**
   `rnk_rootfs::reach` walks `--dockerfile` from `/`.
   - Outside the context, a symlink is followed only when root or
     ranikust's own user owns it: the path is the operator's, and a
     Dockerfile mounted from a Kubernetes ConfigMap is a chain of root's
     symlinks (`Dockerfile -> ..data/Dockerfile`). A link a lower-trust
     account made, for example beside a context it owns, is refused.
   - A link committed to a checkout made by root is root's, and the walk
     cannot tell it from the operator's. A monorepo job with
     `--context=$CI_PROJECT_DIR/services/api` and
     `--dockerfile=$CI_PROJECT_DIR/docker/api.Dockerfile` still follows a
     committed `docker/api.Dockerfile` link. So the parse error shows an
     unknown keyword only when it looks like one (at most 32 ASCII letters,
     digits, `_` or `-`). Otherwise it gives only its length, so a file
     that is no Dockerfile never reaches the error, the log or the metrics
     report. Passing the checkout itself as `--context` closes the read too.
   - Once the walk takes a name inside the context, the rest is resolved
     through the context's `RootPath`, so a committed symlink never leaves
     it. This covers `$CI_PROJECT_DIR/Dockerfile`, the default `Dockerfile`
     from the checkout, and an operator's symlink into the context.
   - A host Dockerfile is read only when it is a regular file.
   - `<Dockerfile>.dockerignore` is read from where the Dockerfile was.
3. **`--context` is never resolved through a symlink.** The directory is
   opened with `RESOLVE_NO_SYMLINKS`, and a symlink in any component is
   refused with a hint to pass the real path.
   - Narrower rules were weighed and rejected. Following only root-owned
     symlinks fails for a checkout root made: `git` creates the committed
     link as root.
   - Following only symlinks whose target lies below the link's own
     directory fails in sticky shared directories (`/tmp/ctx -> root-0700`),
     and needs a resolver of its own.
4. **A hardlink planted into the context is never read.** A context file
   that is a regular file with more than one link, owned by an account other
   than the context directory's owner, is refused.
   - `COPY` and `ADD` from the context check it through
     `CopyOpts::link_owner`. The Dockerfile and the ignore files in the
     context check it through `RootPath::open_context_file`.
   - The check is made on the pinned inode that is then read.
   - Same-owner hardlinks, which a pnpm store makes, are copied as before.
   - Refusing every file the context's owner could not read would close the
     rest. It needs the owner's groups and ACLs, so it is not exact enough
     yet [open: Q-438].

Not covered:

- Kaniko mode on a container's `/` (`Isolation::Direct`), where a `RUN`
  reaches the operator's paths itself (ADR-0078).
- Secrets and registry certificates (ADR-0078).
- A directory outside the context that the same lower-trust account owns.
  Only rules 1 and 4 still apply to an output there: no symlink is
  followed, and nothing is written through another hard link. Choosing to
  write into such a directory is the operator's decision.

### Consequences

- Good, because every redirection above is closed by descriptor, not by a
  check on a path string.
- Good, because a Dockerfile mounted from a ConfigMap, or anywhere else
  outside the context, keeps working.
- Good, because the rules add no flag.
- Bad, because an output in the context is refused. GitLab's habit of
  writing `--digest-file=$CI_PROJECT_DIR/digest` with
  `--context=$CI_PROJECT_DIR` must move the digest or the context, for
  example `--context=$CI_PROJECT_DIR/app`.
- Bad, because a `--context` or an output reached through a symlink is
  refused: git-sync's `/git/repo` link, Fedora Silverblue's
  `/home -> /var/home`, `/var/run -> /run`. The real path works, and the
  message says so.
- Bad, because a context file owned by another account is still read if
  it has a single link. Q-438 tracks closing that.
- Neutral, because `rnk-rootfs` now also reaches operator output paths on
  the host (`OutputDir`, `reach`). That is still "every filesystem access"
  through descriptors, and still never by joining strings onto a host path.

## Evidence

- Source review of the operator paths, 2026-11-02:
  - `write_digest_files` opened with `O_CREAT|O_TRUNC` and no `O_NOFOLLOW`;
  - the layout, tar and metrics writers used `create_dir_all`/`tempfile_in`
    on the parent;
  - `dockerfile_source` sent any path that exists as given to
    `std::fs::File::open`;
  - `DirSource::open` called `std::fs::canonicalize`;
  - `copy_node` reopened every pinned source as root with no owner or
    link-count check.
- Tests, each failing on the code before:
  - `ranikust` `digest_files_never_follow_a_planted_name`;
  - `rnk-builder` `an_output_never_goes_through_a_symlinked_directory`;
  - `rnk-context` `a_context_path_through_a_symlink_is_refused`.
- Tests written with the change:
  - `rnk-rootfs` `host::tests`, `a_context_file_planted_as_a_foreign_hardlink_is_refused`
    and `a_hardlink_owned_by_another_account_is_not_copied_out_of_the_context`;
  - `ranikust` `outputs_in_the_build_context_are_refused`,
    `a_dockerfile_in_the_context_never_follows_a_link_out_of_it`,
    `a_dockerfile_outside_the_context_follows_the_operators_links` and
    `the_metrics_file_stays_out_of_the_context_and_off_symlinks`;
  - the CLI test `a_metrics_file_in_the_context_is_refused`;
  - the T2c test `a_root_only_file_hardlinked_into_the_context_is_never_read`.
- BuildKit reads the context through the client session, as the invoking
  user. kaniko resolves `--dockerfile`, `--context` and its digest files by
  name (`pkg/executor/push.go`, `cmd/executor/cmd/root.go`).
