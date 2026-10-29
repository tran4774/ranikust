// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/instructions/commands_runmount.go,
// commands_rundevice.go, commands_runnetwork.go and
// commands_runsecurity.go at tag v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: Rust port with enums for the closed value sets.

//! `RUN --mount`, `--network`, `--security` and `--device`.

use std::fmt;

use crate::goutil::{csv_fields, parse_bool, parse_uint, ram_in_bytes, to_lower};

/// The type of a `RUN --mount`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum MountType {
    /// `type=bind` (the default): a directory of the context or a stage.
    #[default]
    Bind,
    /// `type=cache`: a persistent cache directory.
    Cache,
    /// `type=tmpfs`.
    Tmpfs,
    /// `type=secret`: a secret file.
    Secret,
    /// `type=ssh`: an SSH agent socket.
    Ssh,
}

impl MountType {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "bind" => Self::Bind,
            "cache" => Self::Cache,
            "tmpfs" => Self::Tmpfs,
            "secret" => Self::Secret,
            "ssh" => Self::Ssh,
            _ => return None,
        })
    }

    /// The name used in the Dockerfile.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bind => "bind",
            Self::Cache => "cache",
            Self::Tmpfs => "tmpfs",
            Self::Secret => "secret",
            Self::Ssh => "ssh",
        }
    }
}

impl fmt::Display for MountType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `sharing=` of a cache mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sharing {
    /// Several builds may use the cache at once.
    Shared,
    /// Each concurrent user gets its own copy.
    Private,
    /// Users wait for each other.
    Locked,
}

impl Sharing {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "shared" => Self::Shared,
            "private" => Self::Private,
            "locked" => Self::Locked,
            _ => return None,
        })
    }

    /// The name used in the Dockerfile.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Private => "private",
            Self::Locked => "locked",
        }
    }
}

/// One `RUN --mount`: BuildKit's `instructions.Mount`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Mount {
    /// `type=`.
    pub kind: MountType,
    /// `from=`: a stage or image to mount from; empty for the context.
    pub from: String,
    /// `source=`, `src=`.
    pub source: String,
    /// `target=`, `dst=`, `destination=`.
    pub target: String,
    /// `readonly`/`ro`, `readwrite`/`rw`; by default cache and tmpfs mounts
    /// are writable and the others read-only.
    pub read_only: bool,
    /// `size=` of a tmpfs mount, in bytes; 0 when unset.
    pub size_limit: i64,
    /// `id=`: the cache or secret id.
    pub cache_id: String,
    /// `sharing=` of a cache mount.
    pub sharing: Option<Sharing>,
    /// `required`: a secret or ssh mount that must exist.
    pub required: bool,
    /// `env=`: the variable a secret is exposed as.
    pub env: Option<String>,
    /// `mode=` (octal) of a cache, secret or ssh mount.
    pub mode: Option<u32>,
    /// `uid=`.
    pub uid: Option<u32>,
    /// `gid=`.
    pub gid: Option<u32>,
}

/// `RUN --network=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Network {
    /// The default network.
    #[default]
    Default,
    /// No network: only loopback.
    None,
    /// The host's network.
    Host,
}

impl Network {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "default" => Self::Default,
            "none" => Self::None,
            "host" => Self::Host,
            _ => return None,
        })
    }

    /// The name used in the Dockerfile.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Host => "host",
        }
    }
}

/// `RUN --security=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Security {
    /// The default sandbox.
    #[default]
    Sandbox,
    /// No sandbox (`--security=insecure`).
    Insecure,
}

impl Security {
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sandbox" => Self::Sandbox,
            "insecure" => Self::Insecure,
            _ => return None,
        })
    }

    /// The name used in the Dockerfile.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sandbox => "sandbox",
            Self::Insecure => "insecure",
        }
    }
}

/// `RUN --device=`: a CDI device.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Device {
    /// The device name.
    pub name: String,
    /// The build fails if the device is missing.
    pub required: bool,
}

/// An expander for mount values: the word in, the expanded word out.
pub(crate) type WordExpander<'a> = &'a mut dyn FnMut(&str) -> Result<String, String>;

/// BuildKit's `parseMount`. Without an expander (at parse time) only the CSV
/// syntax and the absence of variables in `from` are checked, and only
/// `from` is read; with one, every value is expanded and checked.
pub(crate) fn parse_mount(
    val: &str,
    mut expander: Option<WordExpander<'_>>,
) -> Result<Mount, String> {
    let fields = csv_fields(val).map_err(|e| format!("failed to parse csv mounts: {e}"))?;
    let mut m = Mount::default();
    let mut ro_auto = true;

    for field in &fields {
        let (key, value, has_value) = match field.split_once('=') {
            Some((k, v)) => (to_lower(k), v.to_owned(), true),
            None => (to_lower(field), String::new(), false),
        };

        if !has_value {
            if expander.is_none() {
                // Evaluated later.
                continue;
            }
            match key.as_str() {
                "readonly" | "ro" => {
                    m.read_only = true;
                    ro_auto = false;
                    continue;
                }
                "readwrite" | "rw" => {
                    m.read_only = false;
                    ro_auto = false;
                    continue;
                }
                "required" => {
                    if matches!(m.kind, MountType::Secret | MountType::Ssh) {
                        m.required = true;
                        continue;
                    }
                    return Err(format!(
                        "unexpected key '{key}' for mount type '{}'",
                        m.kind
                    ));
                }
                _ => return Err(format!("invalid field '{field}' must be a key=value pair")),
            }
        }

        let value = match expander.as_mut() {
            Some(expand) => expand(&value)?,
            None => {
                if key != "from" {
                    continue;
                }
                // A `$` is allowed only as the last character.
                if value.find('$').is_some_and(|i| i != value.len() - 1) {
                    return Err(format!(
                        "'{key}' doesn't support variable expansion, define alias stage instead"
                    ));
                }
                value
            }
        };

        let invalid = |v: &str| format!("invalid value for {key}: {v}");
        match key.as_str() {
            "type" => {
                m.kind = MountType::parse(&to_lower(&value))
                    .ok_or_else(|| format!("unsupported mount type \"{value}\""))?;
            }
            "from" => m.from = value,
            "source" | "src" => m.source = value,
            "target" | "dst" | "destination" => m.target = value,
            "readonly" | "ro" => {
                m.read_only = parse_bool(&value).ok_or_else(|| invalid(&value))?;
                ro_auto = false;
            }
            "readwrite" | "rw" => {
                m.read_only = !parse_bool(&value).ok_or_else(|| invalid(&value))?;
                ro_auto = false;
            }
            "required" => {
                if !matches!(m.kind, MountType::Secret | MountType::Ssh) {
                    return Err(format!(
                        "unexpected key '{key}' for mount type '{}'",
                        m.kind
                    ));
                }
                m.required = parse_bool(&value).ok_or_else(|| invalid(&value))?;
            }
            "size" => {
                if m.kind != MountType::Tmpfs {
                    return Err(format!(
                        "unexpected key '{key}' for mount type '{}'",
                        m.kind
                    ));
                }
                m.size_limit = ram_in_bytes(&value).map_err(|_| invalid(&value))?;
            }
            "id" => m.cache_id = value,
            "sharing" => {
                m.sharing = Some(
                    Sharing::parse(&to_lower(&value))
                        .ok_or_else(|| format!("unsupported sharing value \"{value}\""))?,
                );
            }
            "mode" => m.mode = Some(parse_u32(&value, 8, "mode")?),
            "uid" => m.uid = Some(parse_u32(&value, 10, "uid")?),
            "gid" => m.gid = Some(parse_u32(&value, 10, "gid")?),
            "env" => m.env = Some(value),
            _ => return Err(format!("unexpected key '{key}' in '{field}'")),
        }
    }

    let file_info_allowed = matches!(
        m.kind,
        MountType::Secret | MountType::Ssh | MountType::Cache
    );
    if !file_info_allowed {
        for (set, what) in [
            (m.mode.is_some(), "mode"),
            (m.uid.is_some(), "uid"),
            (m.gid.is_some(), "gid"),
        ] {
            if set {
                return Err(format!("{what} not allowed for \"{}\" type mounts", m.kind));
            }
        }
    }

    if ro_auto {
        m.read_only = !matches!(m.kind, MountType::Cache | MountType::Tmpfs);
    }

    if m.kind == MountType::Secret {
        if !m.from.is_empty() {
            return Err("secret mount should not have a from".to_owned());
        }
        if m.sharing.is_some() {
            return Err("secret mount should not define sharing".to_owned());
        }
        if m.source.is_empty() && m.target.is_empty() && m.cache_id.is_empty() {
            return Err("invalid secret mount. one of source, target required".to_owned());
        }
        if !m.source.is_empty() && !m.cache_id.is_empty() {
            return Err("both source and id can't be set".to_owned());
        }
    }

    if m.sharing.is_some() && m.kind != MountType::Cache {
        return Err(format!("invalid cache sharing set for {} mount", m.kind));
    }

    Ok(m)
}

fn parse_u32(value: &str, radix: u32, what: &str) -> Result<u32, String> {
    parse_uint(value, radix, 32)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("invalid value {value} for {what}"))
}

/// BuildKit's `ParseDevice`.
pub(crate) fn parse_device(val: &str) -> Result<Device, String> {
    let fields = csv_fields(val).map_err(|e| format!("failed to parse csv devices: {e}"))?;
    let mut d = Device::default();
    for field in &fields {
        let Some((key, value)) = field.split_once('=') else {
            if to_lower(field) == "required" {
                d.required = true;
            } else if d.name.is_empty() {
                d.name.clone_from(field);
            } else {
                return Err(format!("invalid field '{field}' must be a key=value pair"));
            }
            continue;
        };
        match to_lower(key).as_str() {
            "name" => {
                if !d.name.is_empty() {
                    return Err(format!("device name already set to {}", d.name));
                }
                d.name = value.to_owned();
            }
            "required" => {
                d.required = parse_bool(value)
                    .ok_or_else(|| format!("invalid value for required: {value}"))?;
            }
            other => {
                if d.name.is_empty() {
                    d.name.clone_from(field);
                } else {
                    return Err(format!("unexpected key '{other}' in '{field}'"));
                }
            }
        }
    }
    Ok(d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full(val: &str) -> Result<Mount, String> {
        parse_mount(val, Some(&mut |w: &str| Ok(w.to_owned())))
    }

    #[test]
    fn parse_time_reads_only_from() {
        let m = parse_mount("type=cache,target=/c,from=build,mode=xx", None).unwrap();
        assert_eq!(m.kind, MountType::Bind);
        assert_eq!(m.from, "build");
        assert!(m.target.is_empty());
        assert!(parse_mount("from=$X,target=/a", None).is_err());
        assert!(parse_mount("from=x$,target=/a", None).is_ok());
        assert!(parse_mount("", None).unwrap_err().contains("EOF"));
    }

    #[test]
    fn full_parse() {
        let m = full("type=cache,target=/root/.cache,id=pip,sharing=locked,uid=1000,mode=0755")
            .unwrap();
        assert_eq!(m.kind, MountType::Cache);
        assert_eq!(m.cache_id, "pip");
        assert_eq!(m.sharing, Some(Sharing::Locked));
        assert_eq!((m.uid, m.gid, m.mode), (Some(1000), None, Some(0o755)));
        assert!(!m.read_only);

        let m = full("type=secret,id=t,required").unwrap();
        assert!(m.required && m.read_only);
        let m = full("type=tmpfs,target=/t,size=64m").unwrap();
        assert_eq!(m.size_limit, 64 << 20);
        let m = full("target=/ctx,rw=true").unwrap();
        assert_eq!(m.kind, MountType::Bind);
        assert!(!m.read_only);
        let m = full("TYPE=CACHE,target=/c,ro").unwrap();
        assert!(m.read_only && m.kind == MountType::Cache);
    }

    #[test]
    fn full_parse_errors() {
        for (val, msg) in [
            ("type=volume", "unsupported mount type"),
            ("type=cache,foo=bar", "unexpected key 'foo'"),
            ("type=bind,mode=0755", "mode not allowed"),
            ("type=cache,size=1m", "unexpected key 'size'"),
            ("type=secret,id=x,from=y", "should not have a from"),
            ("type=secret", "one of source, target required"),
            ("type=secret,src=a,id=b", "both source and id"),
            ("type=bind,sharing=shared", "invalid cache sharing"),
            ("type=cache,required", "unexpected key 'required'"),
            ("type=cache,uid=-1", "invalid value -1 for uid"),
            ("type=cache,mode=9", "invalid value 9 for mode"),
            ("type=cache,ro=maybe", "invalid value for ro"),
            ("type=cache,bogus", "must be a key=value pair"),
        ] {
            let e = full(val).unwrap_err();
            assert!(e.contains(msg), "{val}: {e}");
        }
    }

    #[test]
    fn devices() {
        assert_eq!(
            parse_device("nvidia.com/gpu=all,required").unwrap(),
            Device {
                name: "nvidia.com/gpu=all".to_owned(),
                required: true
            }
        );
        assert_eq!(parse_device("name=x").unwrap().name, "x");
        assert!(parse_device("a,b").is_err());
        assert!(parse_device("name=a,name=b").is_err());
    }
}
