//! Parsing the `--context` argument into a source kind, as kaniko does.

use std::path::PathBuf;

use crate::{Error, Result};

/// Where the build context comes from, from the `--context` argument.
///
/// kaniko's prefixes (`kaniko@44aa7ddc0 pkg/buildcontext/buildcontext.go:47-71`):
/// an argument without `://` is a local directory
/// (`cmd/executor/cmd/root.go:599-601`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContextSpec {
    /// A local directory, given as a plain path (kaniko's `dir://<path>`
    /// is refused, ADR-0053).
    Dir(PathBuf),
    /// `tar://<path>` or `tar://stdin`: a gzip-compressed tar. Phase 5.
    Tar(String),
    /// `git://<repository>[#ref[#commit]]`. Phase 5.
    Git(String),
    /// `https://<url>` to a tar archive. Phase 5.
    Https(String),
}

/// Parses `--context`.
///
/// Recognizes kaniko's prefixes except `dir://`, which is dropped: a local
/// directory is a plain path (ADR-0053). Cloud object stores (`gs://`, `s3://`)
/// are out of scope (ADR-0008), and an Azure Blob URL is an ordinary
/// `https://` context here.
///
/// # Errors
///
/// [`Error::EmptySpec`] for an empty argument, [`Error::DirScheme`] for
/// `dir://`, [`Error::Unsupported`] for
/// `gs://` and `s3://`, [`Error::UnknownPrefix`] for any other prefix.
pub fn parse_spec(arg: &str) -> Result<ContextSpec> {
    if arg.is_empty() {
        return Err(Error::EmptySpec);
    }
    let Some((prefix, rest)) = arg.split_once("://") else {
        return Ok(ContextSpec::Dir(PathBuf::from(arg)));
    };
    match prefix {
        "dir" => Err(Error::DirScheme {
            path: rest.to_owned(),
        }),
        "tar" => Ok(ContextSpec::Tar(rest.to_owned())),
        "git" => Ok(ContextSpec::Git(rest.to_owned())),
        // kaniko keeps the scheme in the URL it fetches.
        "https" => Ok(ContextSpec::Https(arg.to_owned())),
        "gs" | "s3" => Err(Error::Unsupported {
            kind: "cloud object store",
            reason: "cloud object store contexts are out of scope (ADR-0008)",
        }),
        _ => Err(Error::UnknownPrefix {
            prefix: format!("{prefix}://"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(arg: &str) -> ContextSpec {
        parse_spec(arg).unwrap()
    }

    #[test]
    fn kaniko_prefixes() {
        let dir = |p: &str| ContextSpec::Dir(PathBuf::from(p));
        assert_eq!(parsed("."), dir("."));
        assert_eq!(parsed("/workspace/app"), dir("/workspace/app"));
        assert_eq!(parsed("tar://stdin"), ContextSpec::Tar("stdin".into()));
        assert_eq!(
            parsed("git://github.com/o/r.git#refs/heads/main"),
            ContextSpec::Git("github.com/o/r.git#refs/heads/main".into())
        );
        assert_eq!(
            parsed("https://example.com/ctx.tar.gz"),
            ContextSpec::Https("https://example.com/ctx.tar.gz".into())
        );
    }

    #[test]
    fn refused_prefixes() {
        assert!(matches!(parse_spec(""), Err(Error::EmptySpec)));
        match parse_spec("dir:///workspace") {
            Err(e @ Error::DirScheme { .. }) => {
                assert!(e.to_string().contains("/workspace"), "{e}");
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse_spec("gs://bucket/ctx.tar.gz"),
            Err(Error::Unsupported { .. })
        ));
        assert!(matches!(
            parse_spec("s3://bucket/ctx.tar.gz"),
            Err(Error::Unsupported { .. })
        ));
        match parse_spec("http://example.com/ctx.tar") {
            Err(Error::UnknownPrefix { prefix }) => assert_eq!(prefix, "http://"),
            other => panic!("{other:?}"),
        }
    }
}
