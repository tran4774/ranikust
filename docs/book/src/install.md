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
  --repo tran4774/ranikust
```

The first command checks that the download is complete. The second checks
the tarball's build provenance: that GitHub Actions built it from this
repository's release workflow, from the tagged source. It needs the
[GitHub CLI](https://cli.github.com/), version 2.49 or later.

## Install on a host

```sh
tar -xzf "ranikust-$v-x86_64-unknown-linux-musl.tar.gz"
sudo install -m 0755 "ranikust-$v-x86_64-unknown-linux-musl/ranikust" /usr/local/bin/ranikust
ranikust version
```

The tarball also holds the licences, `NOTICE`, the README and a short
`INSTALL` note. On a host ranikust builds as root, in overlay mode
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
In a
`--privileged` container ranikust uses overlay mode instead, which needs
its state directory on a volume:

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
