---
status: accepted
date: 2026-10-30
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0024: rustls with the `ring` provider; trust host CAs and embedded roots together

## Context and Problem Statement

[ADR-0020](0020-static-musl-via-cargo-zigbuild.md) chose rustls with an
explicitly built trust store that falls back to the embedded webpki roots
only when the host has no CAs, and left the crypto provider (`ring` or
`aws-lc-rs`) to implementation time. Before `rnk-registry` can pull
anything, the provider must be fixed, together with what `oci-client` and
`reqwest` pull in and whether their licences pass `deny.toml` (Phase 1 open
question O4).
Experiment 05 never measured that fallback: the configuration that worked
everywhere was the union of both sets. Which provider, and which trust
store?

## Considered Options

- Provider `aws-lc-rs`, reqwest 0.13's default: a larger static binary and a
  C build through zig; offers a FIPS mode.
- Provider `ring`: the smaller binary.
- Trust only the host's CAs: fails in `scratch`-like images.
- Fall back to the embedded roots only when the host has no CAs (ADR-0020's
  wording): a host bundle holding only a private CA hides every public root.
- Trust only the embedded roots: ignores CAs an operator installed on the
  host.
- Trust the union of the host's CAs and the embedded roots.

## Decision Outcome

ranikust uses rustls with the `ring` crypto provider and trusts the union
of the host's CA bundle and the embedded webpki roots;
`--registry-certificate` adds certificates to that set.

- Host CAs: `/etc/ssl/certs`, `SSL_CERT_FILE` and `SSL_CERT_DIR`, read as
  experiment 05 read them (`rustls-native-certs`). A missing or empty host
  bundle is not an error.
- Embedded roots: `webpki-root-certs`, full DER certificates, because
  `oci-client` accepts only certificate lists (ADR-0020).
- `--registry-certificate` adds CAs, as in kaniko.
- TLS does not need `aws-lc-rs`: the default features of `oci-client`
  (`rustls-tls`) enable `reqwest/rustls`, which selects `aws-lc-rs`, so they
  are turned off; `oci-client`'s `rustls-tls-no-provider` feature does not
  select a provider.
- Licences: `ring` is `Apache-2.0 AND ISC`, `rustls` and
  `rustls-native-certs` `Apache-2.0 OR ISC OR MIT`, `webpki-root-certs`
  `CDLA-Permissive-2.0`; `deny.toml` already allows all of them.

### Consequences

- Good, because the static binary is about 2.1 MB smaller than with
  `aws-lc-rs` on x86_64 and 1.4 MB smaller on aarch64.
- Good, because one configuration works in an empty image (embedded roots)
  and behind a corporate CA (host bundle), with no switch on whether a host
  CA was found.
- Bad, because an operator cannot narrow trust by trimming the host bundle:
  the embedded public roots are always trusted [inferred].
- Bad, because `ring` has no FIPS mode, which `aws-lc-rs` offers [external].
- Bad, because the default features of `oci-client` and `reqwest` bring
  `aws-lc-rs` in as the provider; keeping them off must be checked
  (`cargo tree -i aws-lc-rs`) when `rnk-registry` gains its dependencies.

## Evidence

- [Experiment 05](../research/experiments/05-rust-ecosystem.md#q1-static-binary)
  Q1, Table 2: x86_64 static-pie 5,149,504 B with `ring` against 7,271,488 B
  with `aws-lc-rs`; Table 4: aarch64 4.0 MB against 5.4 MB; both providers
  gave the same results.
- Same experiment, Table 3: the combined store held 118 webpki roots and 121
  host CAs on the host, and 118 + 0 in an empty rootfs, and reached
  `registry-1.docker.io` in every environment; reqwest's default verifier
  failed in an empty rootfs with
  `No CA certificates were loaded from the system`.
- crates.io on 2026-10-30 [external]: `rustls` 0.23.46, `ring` 0.17.14,
  `webpki-root-certs` 1.0.11, `rustls-native-certs` 0.8.5, `aws-lc-rs`
  1.19.0, and their licences as above; `oci-client` 0.18.0 maps
  `rustls-tls` to `reqwest/rustls` and `rustls-tls-no-provider` to
  `reqwest/rustls-no-provider`; reqwest 0.13.5's `rustls` feature enables
  `__rustls-aws-lc-rs`.

## More Information

Refines [ADR-0020](0020-static-musl-via-cargo-zigbuild.md): the union
replaces its fallback rule, and `ring` is the provider it left open. Still
untested from experiment 05: passing the list to `oci-client` through
`tls_certs_only`. Owner: `rnk-registry`
([crate card](../agents/crates/rnk-registry.md)).
