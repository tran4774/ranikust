---
status: proposed
date: 2026-11-02
decision-makers: Tran Nguyen (maintainer)
---

# ADR-0070: Registry references reach `oci-client` built from their parts, and a request fails if it would address another host

## Context and Problem Statement

[ADR-0026](0026-rnk-oci-owns-its-types.md) keeps `oci-spec` types inside
`rnk-registry` and hands references to `oci-client` through their string
form, which `oci-client` parses again with `oci-spec`. The two parsers
split the registry off by different rules. `rnk_oci`, as distribution's
`splitDockerDomain`, takes a first component with an upper-case letter as
the registry (`LOCALDOMAIN/debian` is registry `LOCALDOMAIN`). `oci-spec`
0.10.0 takes a first component as a domain only if it holds `.` or `:` or is
`localhost`, so the same text becomes `docker.io/LOCALDOMAIN/debian`.
`rnk-registry` connects to, checks the realm of and picks the credential for
`LOCALDOMAIN`, then `oci-client` authenticates against `index.docker.io`
with that credential: an operator's credential goes to Docker Hub. The same
holds for `LOCALHOST/app`. Any reference whose two parses disagree has this
shape. A bracketed IPv6 registry (`[fc00::1]:5000/app`, `[::1]:5000/app`)
is another disagreement: `oci-spec` has no `[v6]:port` form, so every such
reference was refused ("invalid reference format") after the `/v2/` ping.

## Considered Options

- Build the `oci-client` reference from `rnk_oci`'s parsed registry,
  repository, tag and digest, and check the host it resolves to.
- Keep the string form and refuse a reference when the two parsers disagree.
- Keep the string form and lower-case or rewrite the registry first: still
  two parsers, and a later `oci-spec` change could split again.

The second option refuses the only spelling that names a single-label,
port-less private registry other than `localhost` (lower case is Docker
Hub by both rules), which distribution and BuildKit accept.

## Decision Outcome

`rnk-registry` builds every `oci-client` reference from the parts `rnk_oci`
parsed (`Reference::with_tag`, `with_digest`, `with_tag_and_digest`), never
by parsing text again, and fails with `Error::Reference`, before any
request, if `oci-client` would still address a host or repository other than
the one the connection and the credential were chosen for.

- One parse, `rnk_oci`'s, decides the registry for the connection, the
  credential lookup, the realm check and every `oci-client` request: pulls,
  blob checks, pushes, mounts and manifest pushes.
- Every call converts its references (the mount source too) before it
  connects or authenticates, so a refused reference sends nothing, not even
  the `/v2/` ping.
- The check compares `resolve_registry()` with the endpoint `connect` used
  (`index.docker.io` for Docker Hub, else the registry as written), and the
  repository, exactly: `oci-client` keys its credentials and tokens on them
  case-sensitively. It holds by construction today; it fails closed if a
  later `oci-client` maps a registry elsewhere.
- Blob requests carry the caller's reference, with whatever tag or digest
  it has (`latest` when it has neither, as `oci-spec`'s parse gave);
  `oci-client` uses only its registry and repository there.
- Otherwise ADR-0026 stands: no `oci-spec` type crosses a crate boundary.

### Consequences

- Good, because a credential goes only to the registry it was chosen for
  and the token realm that registry names.
- Good, because an upper-case, dot-less registry name now works end to end,
  as in distribution and BuildKit, instead of reaching Docker Hub.
- Good, because bracketed IPv6 registries, refused before, now work
  (pulled from `[::1]` in a T0 test).
- Neutral, because references both parsers read alike give the same
  `oci-client` reference as before (tested).
- Bad, because `oci-spec`'s own reference checks (name length, digest
  format) no longer run on these references; `rnk_oci::Reference::parse`
  already checks both.

## Evidence

- [code] `crates/rnk-oci/src/reference.rs` `split_registry`: an upper-case
  first component is the registry. `oci-spec` 0.10.0
  `src/distribution/reference.rs` `split_domain`: only `.`, `:` or
  `localhost`; `resolve_registry` maps `docker.io` to `index.docker.io`.
- [code] `oci-client` 0.18.0 `auth` stores the credential for
  `resolve_registry()` and `_auth` requests `/v2/` of that host, then sends
  a Basic credential to the token realm its challenge names; its token cache
  is keyed on `resolve_registry()` and the repository.
- [measured] T0 `rnk-registry`
  `oci_client_addresses_the_registry_credentials_were_chosen_for`: with the
  string form, `LOCALDOMAIN/debian:1` resolved to `index.docker.io` (the
  test failed). Built from parts, each spelling resolves to a literal
  expected host: `LOCALDOMAIN`, `LOCALHOST`, `Registry`, `localhost`,
  `localhost:5000`, `registry.example.com`, `index.docker.io` for Docker
  Hub, by tag, by digest and by both. Where both parsers agree, the result
  equals `oci-spec`'s parse of the string form. The upper-case spellings'
  string forms are refused by the check
  (`a_reference_oci_client_would_address_elsewhere_is_refused`, also for a
  host differing only in case). `[fc00::1]:5000` and `[::1]:5000` do not
  parse in `oci-spec`.
- [measured] T0 `a_bracketed_ipv6_registry_pulls`: a manifest and a blob
  pulled from a fake registry on `[::1]`; on the previous code it failed
  with "invalid reference format".

## More Information

- Refines [ADR-0026](0026-rnk-oci-owns-its-types.md): its "converts
  references through their string form" no longer holds; the rest of it
  does.
- Card: [`docs/agents/crates/rnk-registry.md`](../agents/crates/rnk-registry.md);
  threat model T11.
