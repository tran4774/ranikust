# Installing

> ranikust is **pre-alpha**: every release so far is a pre-release. Try it,
> report what breaks, and do not rely on it in production yet.

ranikust is one static binary for **x86_64 Linux**, kernel 5.8 or later. It
needs no daemon, container runtime or shared library, so the same file runs on
a host, in a VM and in a container. There is no aarch64 release and no
container image yet; the code is kept buildable and tested on aarch64
([ADR-0009]).

## Download and check

Each [release](https://github.com/tran4774/ranikust/releases) has two files:
`ranikust-<version>-x86_64-unknown-linux-musl.tar.gz` and `SHA256SUMS`. Check
both before you run anything from the tarball:

```sh
v=0.0.1   # the version you downloaded
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify "ranikust-$v-x86_64-unknown-linux-musl.tar.gz" \
  --repo tran4774/ranikust \
  --signer-workflow tran4774/ranikust/.github/workflows/release.yml \
  --source-ref "refs/tags/v$v" \
  --deny-self-hosted-runners
```

The first command checks that the download is complete. The second checks the
build provenance: the tarball was built from the tag `v$v` by this
repository's release workflow on a GitHub-hosted runner ([ADR-0089]).

- Keep all four options. With `--repo` alone, the check accepts an attestation
  made by any workflow of the repository on any branch, so it cannot tell a
  release from a file that a modified workflow attested on a branch.
- Use the [GitHub CLI](https://cli.github.com/) 2.102.0 or later; earlier
  versions match `--signer-workflow` by its beginning only and `--source-ref`
  without regard to case.
- If either check fails, delete the files and do not run them.

## Install on a host

```sh
tar -xzf "ranikust-$v-x86_64-unknown-linux-musl.tar.gz"
sudo install -m 0755 "ranikust-$v-x86_64-unknown-linux-musl/ranikust" /usr/local/bin/ranikust
ranikust version
```

The tarball also holds the licences, `NOTICE`, the README, a short `INSTALL`
note and `THIRD-PARTY-LICENSES` ([ADR-0094]): the notices of every crate
compiled into the binary, of the C code some of them build (mimalloc, zstd,
ring's), and of what the Rust toolchain links in (the standard library, musl
libc, LLVM's libunwind).

On a host ranikust builds as root, in overlay mode
([Choosing where to run it](where-to-run.md)):

```sh
sudo ranikust --context=. --destination=registry.example.com/team/app:1.0
```

## Use it in a container

Bind-mount the binary at `/ranikust/bin/ranikust`, read-only. In kaniko mode,
which a default Docker container or Kubernetes pod gets, ranikust builds on the
container's own `/` and keeps everything under `/ranikust` out of the image, as
kaniko does with `/kaniko`. There the binary must not be reachable through a
writable mount other than the container's root, since a `RUN` is root on that
`/`. ranikust refuses one bound read-write (Docker binds read-write unless told
`:ro`; in Kubernetes set `readOnly: true` on its `volumeMount`), one inside a
directory that is also mounted read-write (put it outside that tree, or mount
the tree read-only too), and a file with other hard links (mount a copy:
cargo's release binaries, such as `target/release/ranikust`, are links of
their copies in `deps/`). A binary built into the container's image needs
no such mount; on a root that is not overlayfs (podman `--rootfs`,
systemd-nspawn, LXC), it too must have one link and no other writable mount
may reach it. Put the build context there too, read-only:

```sh
docker run --rm \
  -v "$PWD/ranikust:/ranikust/bin/ranikust:ro" \
  -v "$PWD/app:/ranikust/ctx:ro" \
  busybox:1.37 \
  /ranikust/bin/ranikust --context=/ranikust/ctx --no-push
```

ranikust prints the image digest on stdout; name a registry with
`--destination` to push instead. Any image that can start the binary will do:
it only provides the container, and kaniko mode builds over its files.

In a `--privileged` container ranikust uses overlay mode instead, which needs
its state directory on a volume:

```sh
docker run --rm --privileged \
  -v "$PWD/ranikust:/ranikust/bin/ranikust:ro" \
  -v "$PWD/app:/ranikust/ctx:ro" \
  -v ranikust-state:/var/lib/ranikust \
  busybox:1.37 \
  /ranikust/bin/ranikust --context=/ranikust/ctx --no-push
```

In kaniko mode on a container's `/`, every `RUN` can read whatever is mounted
into the container, registry credentials included: mount only what the build
needs, and prefer short-lived, push-only credentials. [Command line](usage.md)
lists the flags; [How ranikust builds](how-it-builds.md) what each mode
isolates.

## Upgrading

Download the new release, check it the same way and replace the binary. Before
1.0 a minor version may break flags or behaviour: read the
[changelog](https://github.com/tran4774/ranikust/blob/main/CHANGELOG.md). Only
the latest release receives security fixes, as the
[security policy](https://github.com/tran4774/ranikust/blob/main/SECURITY.md)
says.

[ADR-0009]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0009-x86-64-first-aarch64-ready.md
[ADR-0089]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0089-releases-are-a-release-pr-and-a-tag-built-and-attested-by-a-workflow.md
[ADR-0094]: https://github.com/tran4774/ranikust/blob/main/docs/adr/0094-the-release-ships-third-party-licence-notices-generated-by-cargo-about.md
