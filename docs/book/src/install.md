# Installing

> ranikust is **pre-alpha**: every release so far is a pre-release. Try it,
> report what breaks, and do not rely on it in production yet.

ranikust ships as one static binary for **x86_64 Linux**, kernel 5.8 or
later. It needs no daemon, no container runtime and no shared library, so
the same file runs on a host, in a VM and in a container. There is no
aarch64 release yet: the code is kept buildable and tested on aarch64
([ADR-0009](https://github.com/tran4774/ranikust/blob/main/docs/adr/0009-x86-64-first-aarch64-ready.md)),
and there is no container image yet.

## Download and check

Each [release](https://github.com/tran4774/ranikust/releases) has two
files: `ranikust-<version>-x86_64-unknown-linux-musl.tar.gz` and
`SHA256SUMS`. Check both before running anything from the tarball:

```sh
v=0.0.1   # the version you downloaded
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify "ranikust-$v-x86_64-unknown-linux-musl.tar.gz" \
  --repo tran4774/ranikust \
  --signer-workflow tran4774/ranikust/.github/workflows/release.yml \
  --source-ref "refs/tags/v$v" \
  --deny-self-hosted-runners
```

The first command checks that the download is complete. The second checks
the tarball's build provenance: that it was built from the tag `v$v` by this
repository's release workflow on a GitHub-hosted runner
([ADR-0089](https://github.com/tran4774/ranikust/blob/main/docs/adr/0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md)).
Keep all four options: with `--repo` alone, the check accepts an
attestation made by any workflow of the repository, on any branch, so it
would not tell a release from a file a modified workflow attested on a
branch. It needs the [GitHub CLI](https://cli.github.com/) 2.102.0 or
later: earlier versions match `--signer-workflow` by its beginning only and
`--source-ref` without regard to case. If either check fails, delete the
files and do not run them.

## Install on a host

```sh
tar -xzf "ranikust-$v-x86_64-unknown-linux-musl.tar.gz"
sudo install -m 0755 "ranikust-$v-x86_64-unknown-linux-musl/ranikust" /usr/local/bin/ranikust
ranikust version
```

The tarball also holds ranikust's licences, `NOTICE`, the README, a short
`INSTALL` note and `THIRD-PARTY-LICENSES`
([ADR-0094](https://github.com/tran4774/ranikust/blob/main/docs/adr/0094-the-release-ships-third-party-licence-notices-generated-by-cargo-about.md)):
the licence notices of every crate compiled into the binary and of the C
code some of them build (mimalloc, zstd, ring's), generated from
`Cargo.lock` when the release is built, then those of what the Rust
toolchain links in: the standard library (under Apache-2.0, with Rust's
`COPYRIGHT`), musl libc (its `COPYRIGHT`) and LLVM's libunwind (named,
its licence waiving the notice for binaries).

On a host ranikust builds as root, in overlay mode
([Choosing where to run it](where-to-run.md)):

```sh
sudo ranikust --context=. --destination=registry.example.com/team/app:1.0
```

## Use it in a container

Bind-mount the binary at `/ranikust/bin/ranikust`. In kaniko mode, which a
default Docker container or Kubernetes pod gets, ranikust builds on the
container's own `/` and keeps everything under `/ranikust` out of the image,
as kaniko does with `/kaniko`. Put the build context there too, read-only:

```sh
docker run --rm \
  -v "$PWD/ranikust:/ranikust/bin/ranikust:ro" \
  -v "$PWD/app:/ranikust/ctx:ro" \
  busybox:1.37 \
  /ranikust/bin/ranikust --context=/ranikust/ctx --no-push
```

ranikust prints the image digest on stdout; name a registry with
`--destination` to push instead. Any image that can start the binary will
do: it only provides the container, and kaniko mode builds over its files.

In a `--privileged` container ranikust uses overlay mode instead, which
needs its state directory on a volume:

```sh
docker run --rm --privileged \
  -v "$PWD/ranikust:/ranikust/bin/ranikust:ro" \
  -v "$PWD/app:/ranikust/ctx:ro" \
  -v ranikust-state:/var/lib/ranikust \
  busybox:1.37 \
  /ranikust/bin/ranikust --context=/ranikust/ctx --no-push
```

In kaniko mode on a container's `/`, every `RUN` of the Dockerfile can read
whatever is mounted into the container, registry credentials included:
mount only what the build needs, and prefer short-lived, push-only
credentials. [Command line](usage.md) lists the flags, and
[How ranikust builds](how-it-builds.md) what each mode isolates.

## Upgrading

Download the new release, check it the same way and replace the binary.
Before 1.0 a minor version may break flags or behaviour: read the release
notes, which follow the project's
[changelog](https://github.com/tran4774/ranikust/blob/main/CHANGELOG.md).
Only the latest release receives security fixes
([security policy](https://github.com/tran4774/ranikust/blob/main/SECURITY.md)).
