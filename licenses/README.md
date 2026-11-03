# Licence notices of the toolchain's components

The release binary links, besides crates, parts of the Rust toolchain that
cargo-about does not see. `scripts/release.py notices` appends these files
to `THIRD-PARTY-LICENSES` and checks their SHA-256 first
([ADR-0094](../docs/adr/0094-the-release-ships-third-party-licence-notices-generated-by-cargo-about.md)).
Replace them, with their hashes in `scripts/release.py`, when the pinned
Rust toolchain (`rust-toolchain.toml`) or the musl it bundles changes.

| File | What | Source | SHA-256 |
|---|---|---|---|
| `musl-1.2.5-COPYRIGHT` | musl libc 1.2.5, which Rust 1.99.1 builds for `x86_64-unknown-linux-musl` (`src/ci/docker/scripts/musl.sh` at tag `1.99.1`: `MUSL=musl-1.2.5`, plus CVE patches; the toolchain's `self-contained/libc.a` holds the string `1.2.5`) | `COPYRIGHT` from `https://www.musl-libc.org/releases/musl-1.2.5.tar.gz` (the tarball Rust's script downloads, SHA-256 `a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4`, good signature from musl's key `8364 8929 0BB6 B70F 99FF DA05 56BC DB59 3020 450F`) | `f9bc4423732350eb0b3f7ed7e91d530298476f8fec0c6c427a1c04ade22655af` |
| `rust-1.99.1-COPYRIGHT` | The Rust standard library and compiler-builtins of Rust 1.99.1 | `COPYRIGHT` of `rust-lang/rust` at tag `1.99.1` | `172020dbfd5b53a226dfde77616190a48dcff519b0bc0e6deb91a8450782c4af` |

LLVM's libunwind, also linked, is only named in `THIRD-PARTY-LICENSES`:
its licence, Apache-2.0 WITH LLVM-exception, waives the notice conditions
for code embedded in a binary. The detailed notices of the standard
library's own third-party code are in `COPYRIGHT-library.html`, which
each Rust release ships in `share/doc/rust/`.
