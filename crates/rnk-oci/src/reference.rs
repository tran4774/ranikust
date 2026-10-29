// Derived from distribution/reference, https://github.com/distribution/reference,
// normalize.go, regexp.go, reference.go and their tests (normalize_test.go,
// reference_test.go) at v0.6.0, commit ff14fafe2236e51c2894ac07d4bdfc778e96d682.
// The source files carry no copyright line; copyright the distribution/reference
// authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: the grammar is checked by hand instead of with regular
// expressions, only sha256 and sha512 digests are accepted, a reference keeps
// both its tag and its digest, and the test vectors are adapted to normalized
// parsing.

//! Image references, `[registry/]repository[:tag][@digest]`, normalized as
//! docker/distribution's `ParseNormalizedNamed` does.
//!
//! - A first path component that is not a registry (no `.` or `:`, not
//!   `localhost`, no uppercase letter) means Docker Hub: `alpine` and
//!   `docker.io/alpine` both become `docker.io/library/alpine`.
//! - `index.docker.io`, Docker Hub's legacy name, becomes `docker.io`; other
//!   names, `registry-1.docker.io` included, are kept as written.
//! - The grammar is distribution's: lowercase path components joined by `.`,
//!   `_`, `__` or runs of `-`; a path of at most 255 characters; a tag of
//!   `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`; a host name, an IPv4 address or a
//!   bracketed IPv6 address, with an optional numeric port.

use std::fmt;
use std::str::FromStr;

use crate::digest::Digest;
use crate::error::{Error, excerpt};

/// Longest repository path distribution accepts, in characters.
const MAX_PATH_LEN: usize = 255;
/// Longest tag, in characters.
const MAX_TAG_LEN: usize = 128;

/// A parsed and normalized image reference.
///
/// Both a tag and a digest may be present (`alpine:3.20@sha256:…`); which one
/// a pull uses is the caller's choice. Displays as
/// `registry/repository[:tag][@digest]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Reference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<Digest>,
}

impl Reference {
    /// Docker Hub's registry name after normalization.
    pub const DOCKER_HUB: &'static str = "docker.io";
    /// The tag [`Reference::with_default_tag`] adds.
    pub const DEFAULT_TAG: &'static str = "latest";

    /// Parses and normalizes a reference such as `alpine`,
    /// `ghcr.io/org/app:1.0` or `localhost:5000/app@sha256:…`.
    ///
    /// A reference without tag or digest stays without; see
    /// [`Reference::with_default_tag`].
    pub fn parse(input: &str) -> Result<Self, Error> {
        let fail = |reason| Error::InvalidReference {
            input: excerpt(input),
            reason,
        };
        if input.is_empty() {
            return Err(fail("empty reference"));
        }
        if is_image_id(input) {
            return Err(fail(
                "a 64-character hex string is an image ID, not a repository name",
            ));
        }
        let (registry, remainder) = split_registry(input);
        let (name, digest) = match remainder.split_once('@') {
            Some((name, digest)) => (name, Some(digest)),
            None => (remainder.as_str(), None),
        };
        let (path, tag) = match name.split_once(':') {
            Some((path, tag)) => (path, Some(tag)),
            None => (name, None),
        };
        if path.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(fail("the repository name must be lowercase"));
        }
        if !valid_registry(registry) {
            return Err(fail("invalid registry host or port"));
        }
        if path.len() > MAX_PATH_LEN {
            return Err(fail("the repository name is longer than 255 characters"));
        }
        if !path.split('/').all(valid_path_component) {
            return Err(fail("invalid repository name"));
        }
        if tag.is_some_and(|tag| !valid_tag(tag)) {
            return Err(fail("invalid tag"));
        }
        let digest = digest
            .map(Digest::parse_or_reason)
            .transpose()
            .map_err(fail)?;
        Ok(Self {
            registry: registry.to_owned(),
            repository: path.to_owned(),
            tag: tag.map(str::to_owned),
            digest,
        })
    }

    /// The registry host, with its port if any: `docker.io`,
    /// `localhost:5000`, `[::1]:5000`.
    pub fn registry(&self) -> &str {
        &self.registry
    }

    /// The repository path in the registry: `library/alpine`.
    pub fn repository(&self) -> &str {
        &self.repository
    }

    /// The tag, if any.
    pub fn tag(&self) -> Option<&str> {
        self.tag.as_deref()
    }

    /// The digest, if any.
    pub fn digest(&self) -> Option<&Digest> {
        self.digest.as_ref()
    }

    /// `registry/repository`, without tag or digest.
    pub fn name(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    /// Adds the tag `latest` when the reference has neither tag nor digest,
    /// as distribution's `TagNameOnly` and kaniko do.
    #[must_use]
    pub fn with_default_tag(mut self) -> Self {
        if self.tag.is_none() && self.digest.is_none() {
            self.tag = Some(Self::DEFAULT_TAG.to_owned());
        }
        self
    }

    /// The same repository with another tag.
    pub fn with_tag(mut self, tag: &str) -> Result<Self, Error> {
        if !valid_tag(tag) {
            return Err(Error::InvalidReference {
                input: excerpt(tag),
                reason: "invalid tag",
            });
        }
        self.tag = Some(tag.to_owned());
        Ok(self)
    }

    /// The same repository and tag with a digest.
    #[must_use]
    pub fn with_digest(mut self, digest: Digest) -> Self {
        self.digest = Some(digest);
        self
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.registry, self.repository)?;
        if let Some(tag) = &self.tag {
            write!(f, ":{tag}")?;
        }
        if let Some(digest) = &self.digest {
            write!(f, "@{digest}")?;
        }
        Ok(())
    }
}

impl FromStr for Reference {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// distribution's `anchoredIdentifierRegexp`: `^[a-f0-9]{64}$`.
fn is_image_id(input: &str) -> bool {
    input.len() == 64
        && input
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Splits off the registry and returns it with the rest of the reference,
/// `library/` added for official Docker Hub images (distribution's
/// `splitDockerDomain`).
fn split_registry(input: &str) -> (&str, String) {
    let (registry, rest) = match input.split_once('/') {
        None => (Reference::DOCKER_HUB, input),
        Some(("localhost", rest)) => ("localhost", rest),
        Some(("index.docker.io", rest)) => (Reference::DOCKER_HUB, rest),
        Some((first, rest))
            if first.contains(['.', ':']) || first.bytes().any(|b| b.is_ascii_uppercase()) =>
        {
            (first, rest)
        }
        Some(_) => (Reference::DOCKER_HUB, input),
    };
    let rest = if registry == Reference::DOCKER_HUB && !rest.contains('/') {
        format!("library/{rest}")
    } else {
        rest.to_owned()
    };
    (registry, rest)
}

/// `host[:port]`, where host is a domain name or a bracketed IPv6 address.
fn valid_registry(registry: &str) -> bool {
    let (host_ok, port) = match registry.strip_prefix('[') {
        Some(bracketed) => match bracketed.split_once(']') {
            Some((address, "")) => (valid_ipv6(address), None),
            Some((address, rest)) => match rest.strip_prefix(':') {
                Some(port) => (valid_ipv6(address), Some(port)),
                None => return false,
            },
            None => return false,
        },
        None => match registry.split_once(':') {
            Some((host, port)) => (valid_domain_name(host), Some(port)),
            None => (valid_domain_name(registry), None),
        },
    };
    host_ok && port.is_none_or(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
}

/// distribution's `ipv6address` inside the brackets: `[a-fA-F0-9:]+`.
fn valid_ipv6(address: &str) -> bool {
    !address.is_empty() && address.bytes().all(|b| b.is_ascii_hexdigit() || b == b':')
}

/// Labels of `[a-zA-Z0-9]`, `-` inside, joined by `.`.
fn valid_domain_name(name: &str) -> bool {
    name.split('.').all(|label| {
        let bytes = label.as_bytes();
        match (bytes.first(), bytes.last()) {
            (Some(first), Some(last)) => {
                first.is_ascii_alphanumeric()
                    && last.is_ascii_alphanumeric()
                    && bytes
                        .iter()
                        .all(|b| b.is_ascii_alphanumeric() || *b == b'-')
            }
            (None, _) | (_, None) => false,
        }
    })
}

/// `[a-z0-9]+` runs joined by `.`, `_`, `__` or one or more `-`.
fn valid_path_component(component: &str) -> bool {
    let lower_alnum = |b: &u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    let mut bytes = component.bytes().peekable();
    loop {
        if bytes.next_if(lower_alnum).is_none() {
            return false;
        }
        while bytes.next_if(lower_alnum).is_some() {}
        match bytes.next() {
            None => return true,
            Some(b'.') => {}
            Some(b'_') => {
                // `__` is one separator.
                bytes.next_if_eq(&b'_');
            }
            Some(b'-') => while bytes.next_if_eq(&b'-').is_some() {},
            Some(_) => return false,
        }
    }
}

/// `[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}`.
fn valid_tag(tag: &str) -> bool {
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut bytes = tag.bytes();
    tag.len() <= MAX_TAG_LEN
        && bytes.next().is_some_and(word)
        && bytes.all(|b| word(b) || b == b'.' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA256_F: &str =
        "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

    fn parse(input: &str) -> Reference {
        Reference::parse(input).unwrap_or_else(|e| panic!("{input:?}: {e}"))
    }

    /// Expected registry, repository, tag and digest of a valid input.
    struct Case {
        input: String,
        registry: &'static str,
        repository: String,
        tag: Option<&'static str>,
        digest: Option<String>,
    }

    fn case(input: &str, registry: &'static str, repository: &str) -> Case {
        Case {
            input: input.to_owned(),
            registry,
            repository: repository.to_owned(),
            tag: None,
            digest: None,
        }
    }

    impl Case {
        fn tag(mut self, tag: &'static str) -> Self {
            self.tag = Some(tag);
            self
        }
        fn digest(mut self, digest: &str) -> Self {
            self.digest = Some(digest.to_owned());
            self
        }
    }

    // distribution reference_test.go TestReferenceParse, valid inputs, with
    // the results of normalized parsing.
    #[test]
    fn reference_parse_vectors() {
        let sha512_f = format!("sha512:{}", "f".repeat(128));
        let long_path = format!("{}a", "a/".repeat(127));
        let path_255 = "a".repeat(255);
        let cases = [
            case("test_com", "docker.io", "library/test_com"),
            case("test.com:tag", "docker.io", "library/test.com").tag("tag"),
            case("test.com:5000", "docker.io", "library/test.com").tag("5000"),
            case("test.com/repo:tag", "test.com", "repo").tag("tag"),
            case("test:5000/repo", "test:5000", "repo"),
            case("test:5000/repo:tag", "test:5000", "repo").tag("tag"),
            case(&format!("test:5000/repo@{SHA256_F}"), "test:5000", "repo").digest(SHA256_F),
            case(
                &format!("test:5000/repo:tag@{SHA256_F}"),
                "test:5000",
                "repo",
            )
            .tag("tag")
            .digest(SHA256_F),
            case("lowercase:Uppercase", "docker.io", "library/lowercase").tag("Uppercase"),
            case(
                &format!("{long_path}:tag-puts-this-over-max"),
                "docker.io",
                &long_path,
            )
            .tag("tag-puts-this-over-max"),
            case(
                "sub-dom1.foo.com/bar/baz/quux",
                "sub-dom1.foo.com",
                "bar/baz/quux",
            ),
            case(
                "sub-dom1.foo.com/bar/baz/quux:some-long-tag",
                "sub-dom1.foo.com",
                "bar/baz/quux",
            )
            .tag("some-long-tag"),
            case(
                "b.gcr.io/test.example.com/my-app:test.example.com",
                "b.gcr.io",
                "test.example.com/my-app",
            )
            .tag("test.example.com"),
            case("xn--n3h.com/myimage:xn--n3h.com", "xn--n3h.com", "myimage").tag("xn--n3h.com"),
            case(
                &format!("xn--7o8h.com/myimage:xn--7o8h.com@{sha512_f}"),
                "xn--7o8h.com",
                "myimage",
            )
            .tag("xn--7o8h.com")
            .digest(&sha512_f),
            case("foo_bar.com:8080", "docker.io", "library/foo_bar.com").tag("8080"),
            case("foo/foo_bar.com:8080", "docker.io", "foo/foo_bar.com").tag("8080"),
            case("192.168.1.1", "docker.io", "library/192.168.1.1"),
            case("192.168.1.1:tag", "docker.io", "library/192.168.1.1").tag("tag"),
            case("192.168.1.1:5000", "docker.io", "library/192.168.1.1").tag("5000"),
            case("192.168.1.1/repo", "192.168.1.1", "repo"),
            case("192.168.1.1:5000/repo", "192.168.1.1:5000", "repo"),
            case("192.168.1.1:5000/repo:5050", "192.168.1.1:5000", "repo").tag("5050"),
            case("[2001:db8::1]/repo", "[2001:db8::1]", "repo"),
            case(
                "[2001:db8:1:2:3:4:5:6]/repo:tag",
                "[2001:db8:1:2:3:4:5:6]",
                "repo",
            )
            .tag("tag"),
            case("[2001:db8::1]:5000/repo", "[2001:db8::1]:5000", "repo"),
            case("[2001:db8::1]:5000/repo:tag", "[2001:db8::1]:5000", "repo").tag("tag"),
            case(
                &format!("[2001:db8::1]:5000/repo@{SHA256_F}"),
                "[2001:db8::1]:5000",
                "repo",
            )
            .digest(SHA256_F),
            case(
                &format!("[2001:db8::1]:5000/repo:tag@{SHA256_F}"),
                "[2001:db8::1]:5000",
                "repo",
            )
            .tag("tag")
            .digest(SHA256_F),
            case("[2001:db8::]:5000/repo", "[2001:db8::]:5000", "repo"),
            case("[::1]:5000/repo", "[::1]:5000", "repo"),
            case(
                &format!("example.com/{path_255}:tag"),
                "example.com",
                &path_255,
            )
            .tag("tag"),
        ];
        for c in cases {
            let r = parse(&c.input);
            assert_eq!(r.registry(), c.registry, "{}", c.input);
            assert_eq!(r.repository(), c.repository, "{}", c.input);
            assert_eq!(r.tag(), c.tag, "{}", c.input);
            assert_eq!(
                r.digest().map(Digest::as_str),
                c.digest.as_deref(),
                "{}",
                c.input
            );
            // The normalized form parses to the same reference.
            assert_eq!(parse(&r.to_string()), r, "{}", c.input);
        }
    }

    // distribution reference_test.go TestReferenceParse, invalid inputs.
    #[test]
    fn reference_parse_errors() {
        let cases = [
            String::new(),
            ":justtag".to_owned(),
            format!("@{SHA256_F}"),
            "repo@sha256:ffffffffffffffffffffffffffffffffff".to_owned(),
            format!("validname@invaliddigest:{}", "f".repeat(64)),
            "Uppercase:tag".to_owned(),
            "test:5000/Uppercase/lowercase:tag".to_owned(),
            format!("domain/{}:tag", "a".repeat(256)),
            "aa/asdf$$^/aa".to_owned(),
            "[2001:db8::1]".to_owned(),
            "[2001:db8::1]:5000".to_owned(),
            "[2001:db8::1]:tag".to_owned(),
            "[fe80::1%eth0]:5000/repo".to_owned(),
            "[fe80::1%@invalidzone]:5000/repo".to_owned(),
        ];
        for input in cases {
            assert!(
                matches!(
                    Reference::parse(&input),
                    Err(Error::InvalidReference { .. })
                ),
                "{input:?}"
            );
        }
    }

    // distribution normalize_test.go TestValidateReferenceName and
    // TestValidateRemoteName.
    #[test]
    fn validate_names() {
        let valid = [
            "docker/docker",
            "library/debian",
            "debian",
            "localhost/library/debian",
            "localhost/debian",
            "LOCALDOMAIN/library/debian",
            "LOCALDOMAIN/debian",
            "docker.io/docker/docker",
            "docker.io/library/debian",
            "docker.io/debian",
            "index.docker.io/docker/docker",
            "index.docker.io/library/debian",
            "index.docker.io/debian",
            "127.0.0.1:5000/docker/docker",
            "127.0.0.1:5000/library/debian",
            "127.0.0.1:5000/debian",
            "192.168.0.1",
            "192.168.0.1:80",
            "192.168.0.1:8/debian",
            "192.168.0.2:25000/debian",
            "thisisthesongthatneverendsitgoesonandonandonthisisthesongthatnev",
            "[fc00::1]:5000/docker",
            "[fc00::1]:5000/docker/docker",
            "[fc00:1:2:3:4:5:6:7]:5000/library/debian",
            "docker.io/1a3f5e7d9c1b3a5f7e9d1c3b5a7f9e1d3c5b7a9f1e3d5d7c9b1a3f5e7d9c1b3a",
            "Docker/docker",
            "DOCKER/docker",
            "docker-rules/docker",
            "docker---rules/docker",
            "doc/docker",
            "d/docker",
            "jess/t",
            "dock__er/docker",
        ];
        for input in valid {
            parse(input);
        }
        let too_long_namespace = format!("{}/docker", "this_is_not_a_valid_namespace".repeat(9));
        let invalid = [
            "https://github.com/docker/docker",
            "docker/Docker",
            "-docker",
            "-docker/docker",
            "-docker.io/docker/docker",
            "docker///docker",
            "docker.io/docker/Docker",
            "docker.io/docker///docker",
            "[fc00::1]",
            "[fc00::1]:5000",
            "fc00::1:5000/debian",
            "[fe80::1%eth0]:5000/debian",
            "[2001:db8:3:4::192.0.2.33]:5000/debian",
            "1a3f5e7d9c1b3a5f7e9d1c3b5a7f9e1d3c5b7a9f1e3d5d7c9b1a3f5e7d9c1b3a",
            "docker-/docker",
            "-docker-/docker",
            "____/____",
            "_docker/_docker",
            "dock..er/docker",
            "dock_.er/docker",
            "dock-.er/docker",
            "docker/",
            &too_long_namespace,
        ];
        for input in invalid {
            assert!(Reference::parse(input).is_err(), "{input:?}");
        }
    }

    // distribution normalize_test.go TestParseRepositoryInfo: every spelling
    // of a name normalizes to the same registry, repository and full name.
    #[test]
    fn repository_info() {
        let cases: [(&[&str], &str, &str); 20] = [
            (&["localhost/fooo"], "localhost", "fooo"),
            (&["localhost/fooo/bar"], "localhost", "fooo/bar"),
            (&["LOCALDOMAIN/fooo"], "LOCALDOMAIN", "fooo"),
            (&["LOCALDOMAIN/fooo/bar"], "LOCALDOMAIN", "fooo/bar"),
            (
                &["fooo/bar", "docker.io/fooo/bar", "index.docker.io/fooo/bar"],
                "docker.io",
                "fooo/bar",
            ),
            (
                &["ubuntu", "docker.io/library/ubuntu", "library/ubuntu"],
                "docker.io",
                "library/ubuntu",
            ),
            (
                &["nonlibrary/ubuntu", "docker.io/nonlibrary/ubuntu"],
                "docker.io",
                "nonlibrary/ubuntu",
            ),
            (
                &["other/library", "docker.io/other/library"],
                "docker.io",
                "other/library",
            ),
            (
                &["127.0.0.1:8000/private/moonbase"],
                "127.0.0.1:8000",
                "private/moonbase",
            ),
            (
                &["127.0.0.1:8000/privatebase"],
                "127.0.0.1:8000",
                "privatebase",
            ),
            (
                &["example.com/private/moonbase"],
                "example.com",
                "private/moonbase",
            ),
            (&["example.com/privatebase"], "example.com", "privatebase"),
            (
                &["example.com:8000/private/moonbase"],
                "example.com:8000",
                "private/moonbase",
            ),
            (
                &["example.com:8000/privatebasee"],
                "example.com:8000",
                "privatebasee",
            ),
            (
                &[
                    "ubuntu-12.04-base",
                    "docker.io/library/ubuntu-12.04-base",
                    "index.docker.io/library/ubuntu-12.04-base",
                ],
                "docker.io",
                "library/ubuntu-12.04-base",
            ),
            (
                &["foo", "docker.io/library/foo", "docker.io/foo"],
                "docker.io",
                "library/foo",
            ),
            (
                &["library/foo/bar", "docker.io/library/foo/bar"],
                "docker.io",
                "library/foo/bar",
            ),
            (
                &["store/foo/bar", "docker.io/store/foo/bar"],
                "docker.io",
                "store/foo/bar",
            ),
            (&["Foo/bar"], "Foo", "bar"),
            (&["FOO/bar"], "FOO", "bar"),
        ];
        for (spellings, registry, repository) in cases {
            for input in spellings {
                let r = parse(input);
                assert_eq!(
                    (r.registry(), r.repository()),
                    (registry, repository),
                    "{input}"
                );
                assert_eq!(r.name(), format!("{registry}/{repository}"));
                assert_eq!(r.to_string(), r.name());
            }
        }
    }

    // distribution normalize_test.go TestNormalizedSplitHostname.
    #[test]
    fn split_hostname() {
        let cases = [
            ("test.com/foo", "test.com", "foo"),
            ("test_com/foo", "docker.io", "test_com/foo"),
            ("docker/migrator", "docker.io", "docker/migrator"),
            ("test.com:8080/foo", "test.com:8080", "foo"),
            ("test-com:8080/foo", "test-com:8080", "foo"),
            ("foo", "docker.io", "library/foo"),
            ("xn--n3h.com/foo", "xn--n3h.com", "foo"),
            ("xn--n3h.com:18080/foo", "xn--n3h.com:18080", "foo"),
            ("docker.io/foo", "docker.io", "library/foo"),
            ("docker.io/library/foo", "docker.io", "library/foo"),
            ("docker.io/library/foo/bar", "docker.io", "library/foo/bar"),
        ];
        for (input, registry, repository) in cases {
            let r = parse(input);
            assert_eq!(
                (r.registry(), r.repository()),
                (registry, repository),
                "{input}"
            );
        }
    }

    // distribution normalize_test.go TestParseDockerRef, through
    // with_default_tag. Unlike ParseDockerRef, a tag next to a digest is
    // kept.
    #[test]
    fn default_tag() {
        let digest = "sha256:e6693c20186f837fc393390135d8a598a96a833917917789d63766cab6c59582";
        let cases = [
            (
                "busybox".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "busybox:latest".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                format!("busybox@{digest}"),
                format!("docker.io/library/busybox@{digest}"),
            ),
            (
                "library/busybox".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "docker.io/busybox".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "docker.io/library/busybox".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "docker.io/busybox:latest".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "library/busybox:latest".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "docker.io/library/busybox:latest".to_owned(),
                "docker.io/library/busybox:latest".to_owned(),
            ),
            (
                "gcr.io/library/busybox".to_owned(),
                "gcr.io/library/busybox:latest".to_owned(),
            ),
            (
                format!("gcr.io/library/busybox:latest@{digest}"),
                format!("gcr.io/library/busybox:latest@{digest}"),
            ),
        ];
        for (input, expected) in cases {
            let r = parse(&input).with_default_tag();
            assert_eq!(r.to_string(), expected, "{input}");
            assert_eq!(parse(&expected), r);
        }
        // distribution normalize_test.go TestParseReferenceWithTagAndDigest.
        let short = "busybox:latest@sha256:86e0e091d0da6bde2456dbb48306f3956bbeb2eae1b5b9a43045843f69fe4aaa";
        assert_eq!(
            parse(short).to_string(),
            format!("docker.io/library/{short}")
        );
    }

    #[test]
    fn docker_hub_aliases() {
        for input in [
            "alpine",
            "library/alpine",
            "docker.io/alpine",
            "index.docker.io/alpine",
            "index.docker.io/library/alpine",
        ] {
            let r = parse(input);
            assert_eq!(r.name(), "docker.io/library/alpine", "{input}");
        }
        // Only index.docker.io is an alias; other Docker Hub hosts are kept.
        assert_eq!(
            parse("registry-1.docker.io/library/alpine").registry(),
            "registry-1.docker.io"
        );
        assert_eq!(parse("registry-1.docker.io/alpine").repository(), "alpine");
        assert_eq!(parse("localhost/alpine").registry(), "localhost");
        assert_eq!(parse("localhost:5000/alpine").repository(), "alpine");
        // A bare `localhost` is a repository name, as in distribution.
        assert_eq!(parse("localhost").name(), "docker.io/library/localhost");
    }

    #[test]
    fn tag_and_digest_rules() {
        let tag_128 = format!("t{}", "a".repeat(127));
        assert_eq!(
            parse(&format!("alpine:{tag_128}")).tag(),
            Some(tag_128.as_str())
        );
        let digest_upper = format!("alpine@sha256:{}", "F".repeat(64));
        let sha384 = format!("alpine@sha384:{}", "f".repeat(96));
        for input in [
            format!("alpine:t{}", "a".repeat(128)),
            "alpine:".to_owned(),
            "alpine:.tag".to_owned(),
            "alpine:-tag".to_owned(),
            "alpine:a:b".to_owned(),
            "alpine:tag/x".to_owned(),
            "alpine@".to_owned(),
            "alpine@sha256:".to_owned(),
            digest_upper,
            sha384,
            format!("alpine@{SHA256_F}@{SHA256_F}"),
            "alpine:tag@".to_owned(),
        ] {
            assert!(Reference::parse(&input).is_err(), "{input:?}");
        }
        assert_eq!(parse("alpine:_x.y-Z").tag(), Some("_x.y-Z"));
    }

    #[test]
    fn hostile_inputs_fail_cleanly() {
        let huge = "a".repeat(1 << 20);
        for input in [
            " alpine",
            "alpine ",
            "alp ine",
            "alpine\n",
            "alpine\0",
            "alpiné",
            "ALPINE",
            "/alpine",
            "alpine/",
            "//",
            "a//b",
            "localhost:/alpine",
            "localhost:port/alpine",
            "[]/alpine",
            "[::1/alpine",
            "[::1]x/alpine",
            "host:5000:6000/alpine",
            ".host/alpine",
            "host./alpine",
            "ho_st.com/alpine",
            "@",
            ":",
            &huge,
        ] {
            let err = Reference::parse(input).unwrap_err();
            assert!(err.to_string().len() < 300, "{input:?}");
        }
    }

    #[test]
    fn builders() {
        let digest = Digest::parse(SHA256_F).unwrap();
        let r = parse("ghcr.io/org/app").with_tag("1.0").unwrap();
        assert_eq!(r.to_string(), "ghcr.io/org/app:1.0");
        let r = r.with_digest(digest.clone());
        assert_eq!(r.to_string(), format!("ghcr.io/org/app:1.0@{SHA256_F}"));
        assert_eq!(r.digest(), Some(&digest));
        assert_eq!(r.clone().with_default_tag(), r);
        assert!(parse("app").with_tag("bad tag").is_err());
        let from_str: Reference = "app:1".parse().unwrap();
        assert_eq!(from_str.name(), "docker.io/library/app");
    }
}
