//! The image config: how to run the image, its layer `diff_ids` and history.
//!
//! OCI image-spec v1.1.1
//! [config.md](https://github.com/opencontainers/image-spec/blob/v1.1.1/config.md),
//! plus Docker's extension fields (`Healthcheck`, `OnBuild`, `Shell`), as
//! BuildKit's `dockerspec.DockerOCIImage` types them
//! (`buildkit@cd8ee244 vendor/github.com/moby/docker-image-spec/specs-go/v1/image.go`).
//! Every other property, such as Docker's legacy `Hostname`, `Tty`,
//! `container` and `container_config`, is dropped on parsing (Q-132,
//! `ADR-0032`).
//!
//! Written as BuildKit's typed round trip writes a config: a field Go marks
//! `omitempty` is left out when absent or empty (`""`, `false`, `0`, `[]`,
//! `{}`), and `config` is always written, `{}` when empty
//! (`buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert.go:714`,
//! `vendor/github.com/opencontainers/image-spec/specs-go/v1/config.go`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::descriptor::Platform;
use crate::digest::Digest;
use crate::error::Error;
use crate::serde_util::{null_as_default, omit_empty};

/// An image config (`application/vnd.oci.image.config.v1+json` or
/// `application/vnd.docker.container.image.v1+json`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageConfig {
    /// Creation time, RFC 3339. An input: this crate never reads the clock.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub created: Option<String>,
    /// Author of the image.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub author: Option<String>,
    /// CPU architecture (`GOARCH` spelling). Required by OCI; read as empty
    /// when missing, as go-containerregistry does, and always written.
    #[serde(default)]
    pub architecture: String,
    /// Operating system (`GOOS` spelling). Required by OCI; read as empty
    /// when missing and always written.
    #[serde(default)]
    pub os: String,
    /// Operating system version (Windows).
    #[serde(rename = "os.version", default, skip_serializing_if = "omit_empty")]
    pub os_version: Option<String>,
    /// Required operating system features (Windows).
    #[serde(rename = "os.features", default, skip_serializing_if = "omit_empty")]
    pub os_features: Option<Vec<String>>,
    /// CPU variant (`v8` for `arm64`).
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub variant: Option<String>,
    /// How to run a container from the image; written as `{}` when `None`.
    #[serde(default, serialize_with = "config_or_empty")]
    pub config: Option<ContainerConfig>,
    /// The layers' uncompressed digests.
    #[serde(default)]
    pub rootfs: RootFs,
    /// How each layer was made, base first, including empty layers.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub history: Option<Vec<History>>,
}

/// `config` as Go writes a struct field: always, `{}` when empty.
fn config_or_empty<S: serde::Serializer>(
    config: &Option<ContainerConfig>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match config {
        Some(c) => c.serialize(serializer),
        None => ContainerConfig::default().serialize(serializer),
    }
}

impl ImageConfig {
    /// Parses an image config. Unknown properties are dropped.
    pub fn from_json(bytes: &[u8]) -> Result<Self, Error> {
        Ok(serde_json::from_slice(bytes)?)
    }

    /// The platform the config describes.
    pub fn platform(&self) -> Platform {
        Platform {
            architecture: self.architecture.clone(),
            os: self.os.clone(),
            os_version: self.os_version.clone(),
            os_features: self.os_features.clone(),
            variant: self.variant.clone(),
            features: None,
        }
    }
}

/// The execution parameters of an image: the config's `config` object.
///
/// Field names are Docker's (`PascalCase`). `ExposedPorts` and `Volumes` are
/// JSON objects whose values are `{}`; they are read and written as sets of
/// their keys.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct ContainerConfig {
    /// `USER`: user, or `user:group`, as a name or a number.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub user: Option<String>,
    /// `EXPOSE`: `port/protocol` entries such as `80/tcp`.
    #[serde(default, skip_serializing_if = "omit_empty", with = "key_set")]
    pub exposed_ports: Option<BTreeSet<String>>,
    /// `ENV`: `NAME=value` entries, in order.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub env: Option<Vec<String>>,
    /// `ENTRYPOINT`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub entrypoint: Option<Vec<String>>,
    /// `CMD`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub cmd: Option<Vec<String>>,
    /// `VOLUME`: mount points.
    #[serde(default, skip_serializing_if = "omit_empty", with = "key_set")]
    pub volumes: Option<BTreeSet<String>>,
    /// `WORKDIR`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub working_dir: Option<String>,
    /// `LABEL`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub labels: Option<BTreeMap<String, String>>,
    /// `STOPSIGNAL`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub stop_signal: Option<String>,
    /// Docker: `HEALTHCHECK`; a pointer in Go, so written whenever set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthConfig>,
    /// Docker: `ONBUILD` triggers, as instruction text.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub on_build: Option<Vec<String>>,
    /// Docker: `SHELL`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub shell: Option<Vec<String>>,
    /// Docker: whether `Cmd`/`Entrypoint` are already escaped (Windows);
    /// BuildKit sets it with every `CMD`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub args_escaped: Option<bool>,
}

/// Docker's `HEALTHCHECK` settings. Durations are integer nanoseconds.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct HealthConfig {
    /// `[]` inherit, `["NONE"]` disable, `["CMD", args…]` or
    /// `["CMD-SHELL", command]`.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub test: Option<Vec<String>>,
    /// Time between checks.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub interval: Option<i64>,
    /// Time before a check counts as hung.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub timeout: Option<i64>,
    /// Start-up time during which failures do not count.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub start_period: Option<i64>,
    /// Time between checks during the start period.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub start_interval: Option<i64>,
    /// Consecutive failures before the container is unhealthy.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub retries: Option<i64>,
}

/// The config's `rootfs`: the uncompressed digests of the layers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RootFs {
    /// Always `layers`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Digest of each layer's uncompressed tar, base first. These are not the
    /// manifest's layer digests, which cover the compressed blobs.
    #[serde(deserialize_with = "null_as_default")]
    pub diff_ids: Vec<Digest>,
}

impl Default for RootFs {
    fn default() -> Self {
        Self {
            kind: "layers".to_owned(),
            diff_ids: Vec::new(),
        }
    }
}

/// One entry of the config's `history`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct History {
    /// When the step ran, RFC 3339.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub created: Option<String>,
    /// Author of the step.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub author: Option<String>,
    /// The command that made the step.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub created_by: Option<String>,
    /// A free-form comment.
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub comment: Option<String>,
    /// Whether the step made no layer (and has no `diff_ids` entry).
    #[serde(default, skip_serializing_if = "omit_empty")]
    pub empty_layer: Option<bool>,
}

/// (De)serializes a set of strings as a JSON object whose values are `{}`,
/// Go's `map[string]struct{}`. Values are ignored when read.
mod key_set {
    use std::collections::{BTreeMap, BTreeSet};

    use serde::de::IgnoredAny;
    use serde::ser::SerializeMap;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[derive(Serialize)]
    struct EmptyObject {}

    pub(super) fn serialize<S: Serializer>(
        set: &Option<BTreeSet<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match set {
            None => serializer.serialize_none(),
            Some(set) => {
                let mut map = serializer.serialize_map(Some(set.len()))?;
                for key in set {
                    map.serialize_entry(key, &EmptyObject {})?;
                }
                map.end()
            }
        }
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<BTreeSet<String>>, D::Error> {
        let map = Option::<BTreeMap<String, IgnoredAny>>::deserialize(deserializer)?;
        Ok(map.map(|map| map.into_keys().collect()))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    const DOCKER_CONFIG: &str = include_str!("../testdata/docker-config.json");
    const OCI_CONFIG: &str = include_str!("../testdata/oci-config.json");

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    #[test]
    fn docker_config_keeps_docker_fields() {
        let c = ImageConfig::from_json(DOCKER_CONFIG.as_bytes()).unwrap();
        assert_eq!(c.architecture, "amd64");
        assert_eq!(c.os, "linux");
        let run = c.config.as_ref().unwrap();
        assert_eq!(run.user.as_deref(), Some("nginx:nginx"));
        assert_eq!(
            run.exposed_ports,
            Some(BTreeSet::from(["443/tcp".to_owned(), "80/tcp".to_owned()]))
        );
        assert_eq!(run.cmd, Some(strings(&["nginx", "-g", "daemon off;"])));
        assert_eq!(run.entrypoint, Some(strings(&["/docker-entrypoint.sh"])));
        assert_eq!(run.working_dir.as_deref(), Some("/usr/share/nginx/html"));
        assert_eq!(run.stop_signal.as_deref(), Some("SIGQUIT"));
        assert_eq!(
            run.on_build,
            Some(strings(&["COPY . /usr/share/nginx/html"]))
        );
        assert_eq!(
            run.shell,
            Some(strings(&["/bin/bash", "-o", "pipefail", "-c"]))
        );
        assert_eq!(run.args_escaped, Some(true));
        let health = run.healthcheck.as_ref().unwrap();
        assert_eq!(health.interval, Some(30_000_000_000));
        assert_eq!(health.start_interval, Some(1_000_000_000));
        assert_eq!(health.retries, Some(3));
        assert_eq!(
            run.labels
                .as_ref()
                .unwrap()
                .get("maintainer")
                .map(String::as_str),
            Some("NGINX Docker Maintainers")
        );
        assert_eq!(c.rootfs.kind, "layers");
        assert_eq!(c.rootfs.diff_ids.len(), 2);
        let history = c.history.as_ref().unwrap();
        assert_eq!(history.len(), 4);
        assert_eq!(history[1].empty_layer, Some(true));
        assert_eq!(
            history[3].author.as_deref(),
            Some("NGINX Docker Maintainers")
        );
        assert!(
            history[2]
                .created_by
                .as_deref()
                .unwrap()
                .contains("update && apt-get")
        );
    }

    #[test]
    fn docker_config_round_trip_is_buildkits() {
        // BuildKit's typed round trip: properties outside the OCI and Docker
        // extension specs go, and so do empty values (Q-132).
        let c = ImageConfig::from_json(DOCKER_CONFIG.as_bytes()).unwrap();
        let mut original: Value = serde_json::from_str(DOCKER_CONFIG).unwrap();
        let top = original.as_object_mut().unwrap();
        for k in ["container_config", "container", "docker_version"] {
            top.remove(k);
        }
        let run = top["config"].as_object_mut().unwrap();
        for k in [
            "Hostname",
            "Domainname",
            "AttachStdin",
            "AttachStdout",
            "AttachStderr",
            "Tty",
            "OpenStdin",
            "StdinOnce",
            "Image",
        ] {
            run.remove(k);
        }
        assert_eq!(serde_json::to_value(&c).unwrap(), original);
    }

    #[test]
    fn oci_config_round_trip() {
        let c = ImageConfig::from_json(OCI_CONFIG.as_bytes()).unwrap();
        assert_eq!(c.variant.as_deref(), Some("v8"));
        let platform = c.platform();
        assert_eq!(
            (platform.os.as_str(), platform.architecture.as_str()),
            ("linux", "arm64")
        );
        assert_eq!(platform.variant.as_deref(), Some("v8"));
        let original: Value = serde_json::from_str(OCI_CONFIG).unwrap();
        assert_eq!(serde_json::to_value(&c).unwrap(), original);
    }

    #[test]
    fn legacy_nulls_read_as_absent() {
        // As the classic Docker builder wrote configs.
        let text = json!({
            "architecture": "amd64",
            "os": "linux",
            "config": {
                "Hostname": "", "User": "", "Env": null, "Cmd": null, "Entrypoint": null,
                "Volumes": null, "ExposedPorts": null, "OnBuild": null, "Labels": null,
                "Healthcheck": null, "Shell": null
            },
            "rootfs": {"type": "layers", "diff_ids": null},
            "history": null
        });
        let c = ImageConfig::from_json(text.to_string().as_bytes()).unwrap();
        let run = c.config.as_ref().unwrap();
        assert!(run.env.is_none() && run.volumes.is_none() && run.labels.is_none());
        assert_eq!(run.user.as_deref(), Some(""));
        assert!(c.rootfs.diff_ids.is_empty());
        // Written back without the empty values.
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "architecture": "amd64", "os": "linux", "config": {},
                "rootfs": {"type": "layers", "diff_ids": []}
            })
        );
    }

    #[test]
    fn empty_values_are_left_out_as_go_omitempty() {
        let c = ImageConfig {
            author: Some(String::new()),
            config: Some(ContainerConfig {
                user: Some(String::new()),
                env: Some(Vec::new()),
                labels: Some(BTreeMap::new()),
                exposed_ports: Some(BTreeSet::new()),
                args_escaped: Some(false),
                healthcheck: Some(HealthConfig {
                    test: Some(Vec::new()),
                    interval: Some(0),
                    ..HealthConfig::default()
                }),
                ..ContainerConfig::default()
            }),
            history: Some(vec![History {
                created_by: Some("RUN x".to_owned()),
                author: Some(String::new()),
                empty_layer: Some(false),
                ..History::default()
            }]),
            ..ImageConfig::default()
        };
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "architecture": "", "os": "",
                "config": {"Healthcheck": {}},
                "rootfs": {"type": "layers", "diff_ids": []},
                "history": [{"created_by": "RUN x"}]
            })
        );
    }

    #[test]
    fn missing_properties_take_go_defaults() {
        let c = ImageConfig::from_json(b"{}").unwrap();
        assert_eq!(c, ImageConfig::default());
        assert_eq!(c.rootfs.kind, "layers");
        assert_eq!(
            serde_json::to_value(&c).unwrap(),
            json!({
                "architecture": "", "os": "", "config": {},
                "rootfs": {"type": "layers", "diff_ids": []}
            })
        );
        let c = ImageConfig::from_json(br#"{"rootfs":{}}"#).unwrap();
        assert_eq!(c.rootfs, RootFs::default());
    }

    #[test]
    fn port_and_volume_values_are_ignored() {
        let text = br#"{"config":{"ExposedPorts":{"80/tcp":{"x":1},"53/udp":null},"Volumes":{}}}"#;
        let c = ImageConfig::from_json(text).unwrap();
        let run = c.config.unwrap();
        assert_eq!(
            run.exposed_ports,
            Some(BTreeSet::from(["53/udp".to_owned(), "80/tcp".to_owned()]))
        );
        assert_eq!(run.volumes, Some(BTreeSet::new()));
        let back = serde_json::to_value(&run).unwrap();
        assert_eq!(back, json!({"ExposedPorts": {"53/udp": {}, "80/tcp": {}}}));
    }

    #[test]
    fn key_order_in_input_does_not_matter() {
        // oci-spec 0.10 reads ExposedPorts into a Vec in HashMap order, so
        // two parses of one document could differ; sets and BTreeMaps cannot.
        let a = br#"{"config":{"ExposedPorts":{"1/tcp":{},"2/tcp":{},"3/tcp":{}},"Labels":{"a":"1","b":"2","c":"3"}}}"#;
        let b = br#"{"config":{"Labels":{"c":"3","a":"1","b":"2"},"ExposedPorts":{"3/tcp":{},"1/tcp":{},"2/tcp":{}}}}"#;
        let a = ImageConfig::from_json(a).unwrap();
        let b = ImageConfig::from_json(b).unwrap();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
    }

    #[test]
    fn malformed_configs_are_errors() {
        // serde-derived structs also accept an array of their fields in
        // declaration order, so `[]` reads as the default config; harmless.
        let cases: [&[u8]; 8] = [
            b"",
            b"null",
            br#"{"architecture":1}"#,
            br#"{"rootfs":{"diff_ids":["sha256:abc"]}}"#,
            br#"{"rootfs":{"diff_ids":"sha256:abc"}}"#,
            br#"{"config":{"Env":"PATH=/bin"}}"#,
            br#"{"config":{"Healthcheck":{"Interval":1.5}}}"#,
            br#"{"history":[{"empty_layer":"yes"}]}"#,
        ];
        for case in cases {
            assert!(ImageConfig::from_json(case).is_err(), "{case:?}");
        }
    }
}
