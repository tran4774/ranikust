//! Registry credentials from a docker `config.json`.
//!
//! Reads `auths` entries: the base64 `auth` field (`user:password`), the
//! separate `username` and `password` fields, `identitytoken` (an OAuth2
//! refresh token) and `registrytoken` (a bearer token used as is). Entries
//! are found as the docker CLI finds them: by exact key, then by the host
//! part of a key written as a URL, with Docker Hub's names (`docker.io`,
//! `index.docker.io`, `registry-1.docker.io`) all served by
//! `https://index.docker.io/v1/`.
//!
//! Credential helpers (`credHelpers`, `credsStore`) are recognised but not
//! run before Phase 5 [open: Q-012]: a host they serve falls back to its
//! `auths` entry, if any, and otherwise to anonymous access.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::Deserialize;

use crate::error::Error;

/// The file name inside `DOCKER_CONFIG` or `~/.docker`.
const CONFIG_FILE: &str = "config.json";
/// The key the docker CLI stores Docker Hub credentials under.
const DOCKER_HUB_KEY: &str = "https://index.docker.io/v1/";
/// The host every Docker Hub name is compared as.
const DOCKER_HUB_HOST: &str = "index.docker.io";
/// Upper bound on the size of `config.json`, so a wrong path (a device, a
/// huge file) fails instead of filling memory.
const MAX_CONFIG_BYTES: u64 = 16 << 20;

/// One registry's credential. `Debug` never shows secret values.
#[derive(Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Credential {
    /// User name and password, sent to the token service or as HTTP basic
    /// authentication.
    Basic {
        /// The user name.
        username: String,
        /// The password or personal access token.
        password: String,
    },
    /// An OAuth2 refresh token, exchanged for an access token per repository.
    IdentityToken(String),
    /// A bearer token sent as is.
    RegistryToken(String),
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Basic { .. } => "Basic",
            Self::IdentityToken(_) => "IdentityToken",
            Self::RegistryToken(_) => "RegistryToken",
        };
        write!(f, "{kind}(<redacted>)")
    }
}

/// The credentials of one `config.json`, by registry.
///
/// `Debug` lists registries and credential kinds, never secret values.
#[derive(Clone, Debug, Default)]
pub struct Credentials {
    /// `(key as written, host it is compared as, credential)`, in key order.
    auths: Vec<(String, String, Credential)>,
    /// `credHelpers`: host → helper name.
    helpers: BTreeMap<String, String>,
    /// `credsStore`: the default helper.
    store: Option<String>,
}

impl Credentials {
    /// No credentials: every registry is accessed anonymously.
    pub fn none() -> Self {
        Self::default()
    }

    /// Where the docker CLI looks for `config.json`: `$DOCKER_CONFIG/` if
    /// set and not empty, else `$HOME/.docker/`, as kaniko does.
    ///
    /// Pure: the caller passes the two variables.
    pub fn config_path(docker_config: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
        match docker_config {
            Some(dir) if !dir.is_empty() => Some(Path::new(dir).join(CONFIG_FILE)),
            _ => home.map(|home| home.join(".docker").join(CONFIG_FILE)),
        }
    }

    /// Reads a `config.json`. A missing file means no credentials.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let fail = |reason: String| Error::Credentials {
            path: path.to_owned(),
            reason,
        };
        let meta = match fs::metadata(path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::none()),
            Err(e) => return Err(fail(e.to_string())),
        };
        if !meta.is_file() {
            return Err(fail("not a regular file".to_owned()));
        }
        if meta.len() > MAX_CONFIG_BYTES {
            return Err(fail(format!("larger than {MAX_CONFIG_BYTES} bytes")));
        }
        let bytes = fs::read(path).map_err(|e| fail(e.to_string()))?;
        Self::from_json(&bytes).map_err(fail)
    }

    /// Parses the contents of a `config.json`.
    ///
    /// The error never quotes the file, which holds secrets.
    pub(crate) fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let file: ConfigFile = serde_json::from_slice(bytes).map_err(|e| {
            // serde_json's message may quote a value; keep the position only.
            format!("invalid JSON at line {} column {}", e.line(), e.column())
        })?;
        let mut auths = Vec::new();
        for (key, entry) in file.auths {
            let credential = entry
                .credential()
                .map_err(|reason| format!("auths[{key:?}]: {reason}"))?;
            if let Some(credential) = credential {
                let host = canonical_host(&key);
                auths.push((key, host, credential));
            }
        }
        let helpers = file
            .cred_helpers
            .into_iter()
            .map(|(host, helper)| (canonical_host(&host), helper))
            .collect();
        Ok(Self {
            auths,
            helpers,
            store: file.creds_store.filter(|s| !s.is_empty()),
        })
    }

    /// The credential for `registry` (`rnk_oci::Reference::registry`), if the
    /// file has one in `auths`.
    ///
    /// An exact key wins; otherwise the first key, in sorted order, whose
    /// host matches.
    pub fn get(&self, registry: &str) -> Option<&Credential> {
        let host = canonical_host(registry);
        let exact = self.auths.iter().find(|(key, _, _)| {
            key == registry || (host == DOCKER_HUB_HOST && key == DOCKER_HUB_KEY)
        });
        exact
            .or_else(|| self.auths.iter().find(|(_, h, _)| *h == host))
            .map(|(_, _, credential)| credential)
    }

    /// The credential helper configured for `registry` (`credHelpers`, else
    /// `credsStore`), which Phase 1 does not run [open: Q-012].
    pub fn helper(&self, registry: &str) -> Option<&str> {
        self.helpers
            .get(&canonical_host(registry))
            .or(self.store.as_ref())
            .map(String::as_str)
    }
}

/// The host a key or registry name is compared as: the scheme and any path
/// removed (the docker CLI's `ConvertToHostname`), lowercased, and Docker
/// Hub's names folded into one.
fn canonical_host(name: &str) -> String {
    let rest = name
        .strip_prefix("https://")
        .or_else(|| name.strip_prefix("http://"))
        .unwrap_or(name);
    let host = rest.split('/').next().unwrap_or(rest).to_ascii_lowercase();
    match host.as_str() {
        "docker.io" | "index.docker.io" | "registry-1.docker.io" => DOCKER_HUB_HOST.to_owned(),
        _ => host,
    }
}

/// The parts of `config.json` this crate reads; other keys are ignored.
#[derive(Deserialize)]
struct ConfigFile {
    #[serde(default)]
    auths: BTreeMap<String, AuthEntry>,
    #[serde(default, rename = "credHelpers")]
    cred_helpers: BTreeMap<String, String>,
    #[serde(default, rename = "credsStore")]
    creds_store: Option<String>,
}

/// One `auths` entry. Docker writes `{}` for hosts a helper serves.
#[derive(Deserialize, Default)]
#[serde(default)]
struct AuthEntry {
    auth: Option<String>,
    username: Option<String>,
    password: Option<String>,
    identitytoken: Option<String>,
    registrytoken: Option<String>,
}

impl AuthEntry {
    /// The credential this entry holds, `None` for an empty entry.
    ///
    /// As go-containerregistry does: a registry token first, then an
    /// identity token, then user and password from `auth` or the separate
    /// fields.
    fn credential(self) -> Result<Option<Credential>, &'static str> {
        let present = |s: &Option<String>| s.as_deref().is_some_and(|s| !s.is_empty());
        if present(&self.registrytoken) {
            return Ok(self.registrytoken.map(Credential::RegistryToken));
        }
        if present(&self.identitytoken) {
            return Ok(self.identitytoken.map(Credential::IdentityToken));
        }
        if let Some(auth) = self.auth.filter(|a| !a.is_empty()) {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(auth.trim())
                .map_err(|_| "`auth` is not base64")?;
            let decoded = String::from_utf8(decoded).map_err(|_| "`auth` is not UTF-8")?;
            let (username, password) = decoded
                .split_once(':')
                .ok_or("`auth` is not `user:password`")?;
            return Ok(Some(Credential::Basic {
                username: username.to_owned(),
                password: password.to_owned(),
            }));
        }
        match (self.username, self.password) {
            (Some(username), Some(password)) if !username.is_empty() => {
                Ok(Some(Credential::Basic { username, password }))
            }
            _ => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(username: &str, password: &str) -> Credential {
        Credential::Basic {
            username: username.to_owned(),
            password: password.to_owned(),
        }
    }

    fn b64(text: &str) -> String {
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    #[test]
    fn base64_auth_for_a_local_registry() {
        let json = format!(
            r#"{{"auths":{{"localhost:5055":{{"auth":"{}"}}}}}}"#,
            b64("user:pa:ss")
        );
        let creds = Credentials::from_json(json.as_bytes()).unwrap();
        assert_eq!(creds.get("localhost:5055"), Some(&basic("user", "pa:ss")));
        assert_eq!(creds.get("localhost:5056"), None);
    }

    #[test]
    fn docker_hub_names_share_one_entry() {
        let json = format!(
            r#"{{"auths":{{"https://index.docker.io/v1/":{{"auth":"{}"}}}}}}"#,
            b64("hub:secret")
        );
        let creds = Credentials::from_json(json.as_bytes()).unwrap();
        for name in ["docker.io", "index.docker.io", "registry-1.docker.io"] {
            assert_eq!(creds.get(name), Some(&basic("hub", "secret")), "{name}");
        }
    }

    #[test]
    fn url_keys_match_by_host_and_exact_keys_win() {
        let json = format!(
            r#"{{"auths":{{
                "https://r.example/v2/":{{"auth":"{}"}},
                "r.example":{{"username":"exact","password":"p"}}
            }}}}"#,
            b64("url:p")
        );
        let creds = Credentials::from_json(json.as_bytes()).unwrap();
        assert_eq!(creds.get("r.example"), Some(&basic("exact", "p")));
        assert_eq!(creds.get("R.EXAMPLE"), Some(&basic("url", "p")));
    }

    #[test]
    fn tokens_take_precedence() {
        let json = format!(
            r#"{{"auths":{{
                "id.example":{{"auth":"{}","identitytoken":"refresh"}},
                "reg.example":{{"identitytoken":"refresh","registrytoken":"bearer"}},
                "empty.example":{{}}
            }}}}"#,
            b64("<token>:")
        );
        let creds = Credentials::from_json(json.as_bytes()).unwrap();
        assert_eq!(
            creds.get("id.example"),
            Some(&Credential::IdentityToken("refresh".to_owned()))
        );
        assert_eq!(
            creds.get("reg.example"),
            Some(&Credential::RegistryToken("bearer".to_owned()))
        );
        assert_eq!(creds.get("empty.example"), None);
    }

    #[test]
    fn helpers_are_recognised_but_not_run() {
        let json = r#"{"auths":{"ghcr.example.com":{}},"credHelpers":{"ghcr.example.com":"fake"},"credsStore":"desktop"}"#;
        let creds = Credentials::from_json(json.as_bytes()).unwrap();
        assert_eq!(creds.get("ghcr.example.com"), None);
        assert_eq!(creds.helper("ghcr.example.com"), Some("fake"));
        assert_eq!(creds.helper("other.example.com"), Some("desktop"));
        assert_eq!(Credentials::none().helper("other.example.com"), None);
    }

    #[test]
    fn errors_and_debug_never_show_secrets() {
        let creds = Credentials::from_json(
            br#"{"auths":{"a.example":{"username":"u","password":"hunter2"},"b.example":{"registrytoken":"tok-xyz"}}}"#,
        )
        .unwrap();
        let debug = format!("{creds:?}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("tok-xyz"),
            "{debug}"
        );

        let err =
            Credentials::from_json(br#"{"auths":{"a.example":{"auth":"hunter2!"}}}"#).unwrap_err();
        assert!(!err.contains("hunter2"), "{err}");
        let err =
            Credentials::from_json(br#"{"auths":{"a.example":{"auth":12345678}}}"#).unwrap_err();
        assert!(!err.contains("12345678"), "{err}");
        let err = Credentials::from_json(
            format!(r#"{{"auths":{{"a":{{"auth":"{}"}}}}}}"#, b64("nocolon")).as_bytes(),
        )
        .unwrap_err();
        assert!(!err.contains("nocolon"), "{err}");
    }

    #[test]
    fn config_path_follows_docker_config_then_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            Credentials::config_path(Some(OsStr::new("/cfg")), Some(home)),
            Some(PathBuf::from("/cfg/config.json"))
        );
        assert_eq!(
            Credentials::config_path(Some(OsStr::new("")), Some(home)),
            Some(PathBuf::from("/home/u/.docker/config.json"))
        );
        assert_eq!(Credentials::config_path(None, None), None);
    }

    #[test]
    fn load_reads_files_and_treats_missing_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let missing = Credentials::load(&dir.path().join("config.json")).unwrap();
        assert_eq!(missing.get("r.example"), None);

        let path = dir.path().join("config.json");
        fs::write(
            &path,
            br#"{"auths":{"r.example":{"username":"u","password":"p"}}}"#,
        )
        .unwrap();
        let creds = Credentials::load(&path).unwrap();
        assert_eq!(creds.get("r.example"), Some(&basic("u", "p")));

        let err = Credentials::load(dir.path()).unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }
}
