# Setting up a development machine

Step by step, from a fresh Linux machine to every test tier passing.
[CONTRIBUTING.md](../CONTRIBUTING.md#development-setup) keeps the reference
table of tools and why each is needed; this page is the walkthrough.
Commands are for Ubuntu (24.04 and 26.04) and Debian; other distributions
need the same tools under their own package names.

Nothing in the development loop runs as root. `sudo` appears below only to
install packages and to give yourself access to `/dev/kvm`, once.

## 1. The build (every contributor)

```sh
sudo apt install build-essential pkg-config file binutils curl git jq python3
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # then open a new shell
cargo install --locked just cargo-deny@0.20.2 mdbook@0.5.4 cargo-zigbuild@0.24.0
```

Install zig 0.16.0 into your home as
[CONTRIBUTING.md](../CONTRIBUTING.md#development-setup) shows (one tarball,
checked against Zig's signing key). Then, in the repository:

```sh
just check            # fmt, clippy, T0 tests; rustup installs the pinned toolchain on first use
just build-static     # the static-pie x86_64 musl binary
```

`~/.cargo/bin` and `~/.local/bin` must be on `PATH`.

## 2. aarch64 (when you touch anything platform-specific)

ranikust ships x86_64 first, but aarch64 must keep building and passing its
tests ([ADR-0009](adr/0009-x86-64-first-aarch64-ready.md)), as a static-pie
binary with its symbol table, like x86_64
([ADR-0020](adr/0020-static-musl-via-cargo-zigbuild.md), Q-140).

```sh
sudo apt install gcc-aarch64-linux-gnu qemu-user qemu-user-binfmt
just check-aarch64    # cargo check for aarch64
just test-aarch64     # static-pie aarch64 binary, then the T0 tests under qemu-user
```

- `gcc-aarch64-linux-gnu` is the linker: `just test-aarch64` links with
  `aarch64-linux-gnu-gcc` on x86_64 (and with the native `cc` on an aarch64
  machine, as CI's `ubuntu-24.04-arm` runner does), adding the flags that
  give static-pie with musl's own start files (rustc's aarch64 musl target
  does not do it by itself, Q-140). Without it the recipe falls back to
  zig's linker, warns, and cannot check static-pie.
- `qemu-user` must be QEMU 9.1 or later (`qemu-aarch64 --version`): older
  ones have no `openat2`. Ubuntu 26.04 ships 10.2; Ubuntu 24.04 ships 8.2,
  so there unpack Debian 13's package instead, without installing it:

  ```sh
  deb=qemu-user_10.0.13+ds-0+deb13u1_amd64.deb
  curl -fLO "https://deb.debian.org/debian/pool/main/q/qemu/$deb"
  echo "ca6ede739327a20ae5a3498230c8a3a9a072c586e131bc6bcc1201eb1725e336  $deb" | sha256sum -c -
  dpkg -x "$deb" ~/.local/share/qemu-user
  export QEMU_AARCH64=~/.local/share/qemu-user/usr/bin/qemu-aarch64
  ```

- `qemu-user-binfmt` lets the kernel run aarch64 binaries directly, so the
  tests that spawn the binary (ranikust's `cli` and `metrics`, rnk-store's
  `processes`) run too; without it the recipe skips them with a warning.
- On an aarch64 machine none of the emulation is needed.

## 3. Rootless containers (T1 smoke tests, test registries)

```sh
sudo apt install podman podman-compose skopeo openssl python3-bcrypt
just smoke            # the smoke cases in rootless podman
just registry-up      # HTTP and self-signed HTTPS test registries (tests/registry)
just registry-down
```

Rootless podman needs subordinate ids for your user (`grep $USER
/etc/subuid /etc/subgid`); most distributions add them when the user is
created.

## 4. The disposable VM (T2: anything that needs real root)

Real root never runs on your machine: `just test-vm` boots a throwaway
QEMU/KVM VM as you ([testing](testing.md#the-vm-harness-tier-t2)).

```sh
sudo apt install qemu-system-x86 qemu-utils xorriso openssh-client
sudo usermod -aG kvm "$USER"      # once; log out and in, then check:
test -r /dev/kvm -a -w /dev/kvm && echo "kvm ok"
just test-vm unit                 # first run downloads the Ubuntu cloud image
```

- The verified cloud images are cached in `tests/e2e/_vm-image/` of the main
  checkout (gitignored) and shared by every git worktree; only one VM runs
  at a time, a second run waits for the first.
- A VM takes 12 vCPUs and 16 GiB (`RANIKUST_VM_CPUS`/`_MEM`), shrunk to
  what the host can spare (at least 4 GiB, 8 GiB for `e2e` and `ultra`);
  the first run after a new cloud image builds a base image with Docker
  installed, a few minutes, once.
- Day to day, VM smoke tests and proofs of concept use the small cases in
  `tests/smoke/cases`; the large nextjs-kaniko workloads (`just test-vm
  ultra`) are only for benchmarks and acceptance measurements the
  maintainer asks for (docs/testing.md#which-test-for-what).

## 5. End-to-end benchmarks (optional)

`just test-vm e2e`, the light e2e, needs only the kaniko
(`kaniko-bench:stream`), BuildKit and `registry:2` images in your podman.
`just test-vm ultra`, the ultra benchmark (only when the maintainer asks for
it), also needs a
[`nextjs-kaniko`](testing.md#end-to-end-workloads-e2e) checkout next to the
repository (or `RANIKUST_NEXTJS_KANIKO=<path>`), its registry and npm-proxy
storage filled, and its verdaccio image in your podman. About 3 GB is
copied into the VM per ultra run.

## Check the setup

| Command | Proves |
|---|---|
| `just check` | the build and T0 tests |
| `just build-static` | static-pie x86_64 binary |
| `just test-aarch64` | static-pie aarch64 binary and its tests |
| `just smoke` | rootless podman (T1) |
| `just test-vm unit` | KVM access and the VM harness (T2) |
| `just ci` | everything CI runs |

If a recipe says a tool is missing, its row in
[CONTRIBUTING.md](../CONTRIBUTING.md#development-setup) says which package
provides it.
