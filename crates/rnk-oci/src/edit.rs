// The signal-name table and `valid_signal` are derived from moby/sys,
// https://github.com/moby/sys, signal/signal.go and signal/signal_linux.go
// at signal/v0.7.1 (vendored by kaniko 44aa7ddc0).
// Copyright The moby/sys authors (no copyright line in the source files).
// Licensed under the Apache License, Version 2.0.
// Modified for ranikust: ParseSignal reduced to a validity check, the
// RTMIN/RTMAX names spelled out as strings.

//! Config edits: what each metadata instruction does to the image config.
//!
//! Values arrive expanded (`ARG`/`ENV` substitution is `rnk-dockerfile`'s
//! and `rnk-builder`'s job); an edit only applies them, as kaniko's
//! `ExecuteCommand`s do (`kaniko@44aa7ddc0 pkg/commands/*.go`). Where kaniko
//! and BuildKit differ, the edit follows kaniko [open: Q-130]. Empty
//! `Entrypoint`, `Cmd` and a false `ArgsEscaped` are left out, as both write
//! them (`omitempty`).
//!
//! Only the config changes here. Creating the `WORKDIR` directory (owned by
//! the active user) and the `VOLUME` directories, as kaniko does, is
//! `rnk-builder`'s job, through `rnk_rootfs::RootPath`.
//!
//! History entries and `rootfs.diff_ids` change through
//! [`ImageConfig::push_layer`] and [`ImageConfig::push_history`]; which
//! instructions get an entry is the builder's choice [open: Q-131].

use crate::config::{ContainerConfig, HealthConfig, History, ImageConfig};
use crate::digest::Digest;
use crate::error::{Error, excerpt};

/// The shell a shell-form `CMD` or `ENTRYPOINT` runs with when the config
/// has no `Shell`, as kaniko and BuildKit on Linux.
pub const DEFAULT_SHELL: [&str; 2] = ["/bin/sh", "-c"];

/// A `CMD` or `ENTRYPOINT` command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandLine {
    /// Exec form: the arguments as written. Empty clears the field.
    Exec(Vec<String>),
    /// Shell form: the line, run with the config's `Shell`.
    Shell(String),
}

/// The change one metadata instruction makes to an image config, with every
/// value already expanded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigEdit {
    /// `ENV`: each key replaces its entry in place, or is appended. Not
    /// empty.
    Env(Vec<(String, String)>),
    /// `LABEL`: each key is set, later ones winning. Not empty.
    Label(Vec<(String, String)>),
    /// `USER`: `user` or `user:group`; past a second `:` the rest is dropped,
    /// as kaniko does (`pkg/commands/user.go`).
    User(String),
    /// `WORKDIR`: absolute as written, or relative to the current one.
    Workdir(String),
    /// `EXPOSE`: `port`, `port/tcp` or `port/udp`; no protocol means `tcp`.
    /// Not empty.
    Expose(Vec<String>),
    /// `VOLUME`: mount points, added to the set. Not empty.
    Volume(Vec<String>),
    /// `ENTRYPOINT`.
    Entrypoint(CommandLine),
    /// `CMD`.
    Cmd(CommandLine),
    /// `SHELL`.
    Shell(Vec<String>),
    /// `STOPSIGNAL`: a signal name (`SIGTERM`, `term`) or a non-zero number,
    /// stored as written.
    StopSignal(String),
    /// `HEALTHCHECK`; durations in nanoseconds. A zero duration or retry
    /// count is stored as `None`, as go-containerregistry omits it.
    Healthcheck(HealthConfig),
    /// `ONBUILD`: the trigger's text, appended.
    OnBuild(String),
}

impl ConfigEdit {
    /// The instruction name, upper case, for messages.
    pub fn instruction(&self) -> &'static str {
        match self {
            Self::Env(_) => "ENV",
            Self::Label(_) => "LABEL",
            Self::User(_) => "USER",
            Self::Workdir(_) => "WORKDIR",
            Self::Expose(_) => "EXPOSE",
            Self::Volume(_) => "VOLUME",
            Self::Entrypoint(_) => "ENTRYPOINT",
            Self::Cmd(_) => "CMD",
            Self::Shell(_) => "SHELL",
            Self::StopSignal(_) => "STOPSIGNAL",
            Self::Healthcheck(_) => "HEALTHCHECK",
            Self::OnBuild(_) => "ONBUILD",
        }
    }

    /// Applies the edit to `config`, creating its `config` object if needed.
    ///
    /// On error `config` is unchanged.
    pub fn apply(&self, config: &mut ImageConfig) -> Result<(), Error> {
        self.validate()?;
        let run = config.config.get_or_insert_with(ContainerConfig::default);
        match self {
            Self::Env(pairs) => set_env(run, pairs),
            Self::Label(pairs) => {
                let labels = run.labels.get_or_insert_with(Default::default);
                for (k, v) in pairs {
                    labels.insert(k.clone(), v.clone());
                }
            }
            Self::User(user) => run.user = Some(user_and_group(user)),
            Self::Workdir(dir) => {
                run.working_dir = Some(workdir(run.working_dir.as_deref().unwrap_or(""), dir));
            }
            Self::Expose(ports) => {
                let set = run.exposed_ports.get_or_insert_with(Default::default);
                for p in ports {
                    set.insert(if p.contains('/') {
                        p.clone()
                    } else {
                        format!("{p}/tcp")
                    });
                }
            }
            Self::Volume(volumes) => {
                let set = run.volumes.get_or_insert_with(Default::default);
                set.extend(volumes.iter().cloned());
            }
            Self::Entrypoint(command) => {
                run.entrypoint = command_line(command, run.shell.as_deref())
            }
            Self::Cmd(command) => {
                run.cmd = command_line(command, run.shell.as_deref());
                // kaniko writes `false`, which `omitempty` drops.
                run.args_escaped = None;
            }
            Self::Shell(shell) => run.shell = Some(shell.clone()),
            Self::StopSignal(signal) => run.stop_signal = Some(signal.clone()),
            Self::Healthcheck(health) => run.healthcheck = Some(without_zeros(health)),
            Self::OnBuild(trigger) => run
                .on_build
                .get_or_insert_with(Vec::new)
                .push(trigger.clone()),
        }
        Ok(())
    }

    /// Checks the values kaniko rejects, before anything changes.
    fn validate(&self) -> Result<(), Error> {
        let invalid = |input: &str, reason| Error::InvalidEdit {
            instruction: self.instruction(),
            input: excerpt(input),
            reason,
        };
        // kaniko writes nothing for an empty list; refuse it rather than
        // write `{}` or `[]`.
        let empty = |is_empty: bool| {
            if is_empty {
                Err(invalid("", "needs at least one value"))
            } else {
                Ok(())
            }
        };
        match self {
            Self::Env(pairs) | Self::Label(pairs) => empty(pairs.is_empty())?,
            Self::Volume(volumes) => empty(volumes.is_empty())?,
            // `pkg/commands/expose.go:47-62`: tcp or udp, exactly one `/`.
            Self::Expose(ports) => {
                empty(ports.is_empty())?;
                for p in ports {
                    if let Some((_, protocol)) = p.split_once('/') {
                        if protocol.contains('/') {
                            return Err(invalid(p, "more than one '/'"));
                        }
                        if protocol != "tcp" && protocol != "udp" {
                            return Err(invalid(p, "the protocol must be tcp or udp"));
                        }
                    }
                }
            }
            Self::StopSignal(signal) => {
                if !valid_signal(signal) {
                    return Err(invalid(signal, "not a signal name or a non-zero number"));
                }
            }
            Self::Shell(shell) => empty(shell.is_empty())?,
            Self::User(_)
            | Self::Workdir(_)
            | Self::Entrypoint(_)
            | Self::Cmd(_)
            | Self::Healthcheck(_)
            | Self::OnBuild(_) => {}
        }
        Ok(())
    }
}

impl ImageConfig {
    /// Appends a layer: its uncompressed digest to `rootfs.diff_ids` and its
    /// history entry, with `empty_layer` unset.
    pub fn push_layer(&mut self, diff_id: Digest, mut history: History) {
        history.empty_layer = None;
        self.rootfs.diff_ids.push(diff_id);
        self.history.get_or_insert_with(Vec::new).push(history);
    }

    /// Appends a history entry for a step that made no layer.
    pub fn push_history(&mut self, mut history: History) {
        history.empty_layer = Some(true);
        self.history.get_or_insert_with(Vec::new).push(history);
    }
}

/// kaniko's `USER` (`pkg/commands/user.go`): it splits on `:` and keeps the
/// first two parts.
fn user_and_group(user: &str) -> String {
    let mut parts = user.split(':');
    match (parts.next(), parts.next()) {
        (Some(u), Some(g)) => format!("{u}:{g}"),
        _ => user.to_owned(),
    }
}

/// `health` with zero durations and retries as `None`: go-containerregistry
/// writes them `omitempty`.
fn without_zeros(health: &HealthConfig) -> HealthConfig {
    let nonzero = |v: Option<i64>| v.filter(|&n| n != 0);
    HealthConfig {
        test: health.test.clone(),
        interval: nonzero(health.interval),
        timeout: nonzero(health.timeout),
        start_period: nonzero(health.start_period),
        start_interval: nonzero(health.start_interval),
        retries: nonzero(health.retries),
    }
}

/// kaniko's `UpdateConfigEnv` (`pkg/util/command_util.go:322-370`): keys
/// match exactly; every entry is rewritten as `key=value`, so a base entry
/// without `=` gains one.
fn set_env(run: &mut ContainerConfig, pairs: &[(String, String)]) {
    let mut entries: Vec<(String, String)> = run
        .env
        .iter()
        .flatten()
        .map(|e| match e.split_once('=') {
            Some((k, v)) => (k.to_owned(), v.to_owned()),
            None => (e.clone(), String::new()),
        })
        .collect();
    for (key, value) in pairs {
        match entries.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1.clone_from(value),
            None => entries.push((key.clone(), value.clone())),
        }
    }
    run.env = Some(
        entries
            .into_iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect(),
    );
}

/// kaniko's `ToAbsPath` (`pkg/commands/workdir.go:39-51`): an absolute path
/// is kept as written; a relative one is joined to the current directory
/// (rooted at `/`) and cleaned as Go's `filepath.Join` does.
fn workdir(current: &str, requested: &str) -> String {
    if requested.starts_with('/') {
        return requested.to_owned();
    }
    clean(&format!("/{current}/{requested}"))
}

/// Go's `path.Clean` for a rooted path.
fn clean(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

/// The command a `CMD` or `ENTRYPOINT` sets; `None` for an empty exec form,
/// which kaniko and BuildKit leave out.
fn command_line(command: &CommandLine, shell: Option<&[String]>) -> Option<Vec<String>> {
    match command {
        CommandLine::Exec(args) if args.is_empty() => None,
        CommandLine::Exec(args) => Some(args.clone()),
        CommandLine::Shell(line) => {
            let mut argv: Vec<String> = match shell {
                Some(shell) if !shell.is_empty() => shell.to_vec(),
                _ => DEFAULT_SHELL.iter().map(|&s| s.to_owned()).collect(),
            };
            argv.push(line.clone());
            Some(argv)
        }
    }
}

/// Linux signal names, without `SIG`, as moby's `SignalMap`
/// (`kaniko@44aa7ddc0 vendor/github.com/moby/sys/signal/signal_linux.go`).
const SIGNALS: [&str; 65] = [
    "ABRT", "ALRM", "BUS", "CHLD", "CLD", "CONT", "FPE", "HUP", "ILL", "INT", "IO", "IOT", "KILL",
    "PIPE", "POLL", "PROF", "PWR", "QUIT", "SEGV", "STKFLT", "STOP", "SYS", "TERM", "TRAP", "TSTP",
    "TTIN", "TTOU", "URG", "USR1", "USR2", "VTALRM", "WINCH", "XCPU", "XFSZ", "RTMIN", "RTMIN+1",
    "RTMIN+2", "RTMIN+3", "RTMIN+4", "RTMIN+5", "RTMIN+6", "RTMIN+7", "RTMIN+8", "RTMIN+9",
    "RTMIN+10", "RTMIN+11", "RTMIN+12", "RTMIN+13", "RTMIN+14", "RTMIN+15", "RTMAX-14", "RTMAX-13",
    "RTMAX-12", "RTMAX-11", "RTMAX-10", "RTMAX-9", "RTMAX-8", "RTMAX-7", "RTMAX-6", "RTMAX-5",
    "RTMAX-4", "RTMAX-3", "RTMAX-2", "RTMAX-1", "RTMAX",
];

/// moby's `ParseSignal`: a non-zero integer (Go's `Atoi`: an optional sign
/// and decimal digits, as `i64::from_str`), or a name from the Linux table,
/// case-insensitive, with or without `SIG`.
fn valid_signal(raw: &str) -> bool {
    if let Ok(n) = raw.parse::<i64>() {
        return n != 0;
    }
    let upper = raw.to_ascii_uppercase();
    let name = upper.strip_prefix("SIG").unwrap_or(&upper);
    SIGNALS.contains(&name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::canonical::to_canonical_json;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|&s| s.to_owned()).collect()
    }

    fn kv(k: &str, v: &str) -> (String, String) {
        (k.to_owned(), v.to_owned())
    }

    fn applied(edits: &[ConfigEdit]) -> ImageConfig {
        let mut c = ImageConfig::default();
        for e in edits {
            e.apply(&mut c).unwrap();
        }
        c
    }

    fn run(c: &ImageConfig) -> &ContainerConfig {
        c.config.as_ref().unwrap()
    }

    #[test]
    fn env_replaces_in_place_and_appends() {
        let mut c = ImageConfig {
            config: Some(ContainerConfig {
                env: Some(s(&["PATH=/bin", "A=1", "BARE", "B=x=y"])),
                ..Default::default()
            }),
            ..Default::default()
        };
        ConfigEdit::Env(vec![
            kv("A", "2"),
            kv("C", "3"),
            kv("PATH", "/usr/bin:/bin"),
            kv("C", "4"),
        ])
        .apply(&mut c)
        .unwrap();
        assert_eq!(
            run(&c).env,
            Some(s(&["PATH=/usr/bin:/bin", "A=2", "BARE=", "B=x=y", "C=4"]))
        );
        // Keys are case-sensitive on Linux.
        ConfigEdit::Env(vec![kv("a", "lower")])
            .apply(&mut c)
            .unwrap();
        assert_eq!(run(&c).env.as_ref().unwrap().last().unwrap(), "a=lower");
    }

    #[test]
    fn labels_volumes_ports_accumulate() {
        let c = applied(&[
            ConfigEdit::Label(vec![kv("b", "1"), kv("a", "x")]),
            ConfigEdit::Label(vec![kv("b", "2")]),
            ConfigEdit::Volume(s(&["/data"])),
            ConfigEdit::Volume(s(&["/cache", "/data"])),
            ConfigEdit::Expose(s(&["80", "53/udp"])),
            ConfigEdit::Expose(s(&["80/tcp", "8000-8001"])),
        ]);
        let value = serde_json::to_value(run(&c)).unwrap();
        assert_eq!(
            value,
            json!({
                "ExposedPorts": {"53/udp": {}, "80/tcp": {}, "8000-8001/tcp": {}},
                "Labels": {"a": "x", "b": "2"},
                "Volumes": {"/cache": {}, "/data": {}}
            })
        );
    }

    #[test]
    fn expose_rejects_what_kaniko_rejects() {
        for bad in ["80/sctp", "80/TCP", "80/tcp/udp", "80/"] {
            let mut c = ImageConfig::default();
            let err = ConfigEdit::Expose(s(&["443", bad]))
                .apply(&mut c)
                .unwrap_err();
            assert!(err.to_string().contains("EXPOSE"), "{err}");
            assert_eq!(c, ImageConfig::default(), "unchanged on error: {bad}");
        }
    }

    #[test]
    fn workdir_follows_kaniko() {
        let cases = [
            ("", "/app", "/app"),
            ("", "app", "/app"),
            ("/app", "src", "/app/src"),
            ("/app", "../etc/./x/", "/etc/x"),
            ("/", "../../..", "/"),
            ("rel", "x", "/rel/x"),
            // Absolute paths are kept as written, not cleaned.
            ("/app", "/a/../b/", "/a/../b/"),
        ];
        for (current, requested, want) in cases {
            assert_eq!(workdir(current, requested), want, "{current} + {requested}");
        }
        let c = applied(&[
            ConfigEdit::Workdir("/srv".to_owned()),
            ConfigEdit::Workdir("www".to_owned()),
        ]);
        assert_eq!(run(&c).working_dir.as_deref(), Some("/srv/www"));
    }

    #[test]
    fn shell_form_uses_the_config_shell() {
        let c = applied(&[
            ConfigEdit::Cmd(CommandLine::Shell("echo hi".to_owned())),
            ConfigEdit::Shell(s(&["/bin/bash", "-eo", "pipefail", "-c"])),
            ConfigEdit::Entrypoint(CommandLine::Shell("exec app".to_owned())),
        ]);
        assert_eq!(run(&c).cmd, Some(s(&["/bin/sh", "-c", "echo hi"])));
        assert_eq!(
            run(&c).entrypoint,
            Some(s(&["/bin/bash", "-eo", "pipefail", "-c", "exec app"]))
        );
    }

    #[test]
    fn cmd_and_entrypoint_follow_kaniko() {
        let mut c = ImageConfig {
            config: Some(ContainerConfig {
                cmd: Some(s(&["nginx"])),
                args_escaped: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        // kaniko keeps the inherited CMD under a new ENTRYPOINT [open: Q-130].
        ConfigEdit::Entrypoint(CommandLine::Exec(s(&["/init"])))
            .apply(&mut c)
            .unwrap();
        assert_eq!(run(&c).cmd, Some(s(&["nginx"])));
        assert_eq!(run(&c).args_escaped, Some(true));
        ConfigEdit::Cmd(CommandLine::Exec(Vec::new()))
            .apply(&mut c)
            .unwrap();
        assert_eq!(run(&c).cmd, None);
        assert_eq!(run(&c).args_escaped, None);
        ConfigEdit::Entrypoint(CommandLine::Exec(Vec::new()))
            .apply(&mut c)
            .unwrap();
        assert_eq!(serde_json::to_value(run(&c)).unwrap(), json!({}));
    }

    #[test]
    fn signals_follow_moby() {
        for ok in [
            "SIGTERM",
            "term",
            "SigQuit",
            "KILL",
            "9",
            "-1",
            "+15",
            "RTMIN",
            "SIGRTMIN+15",
            "rtmax-14",
            "RTMAX",
        ] {
            assert!(valid_signal(ok), "{ok}");
        }
        for bad in [
            "", "0", "SIGFOO", "RTMIN+16", "RTMIN+0", "RTMAX-15", "RTMIN+01", "SIG", "9x",
            "RTMIN++5", "RTMAX-+3", "RTMIN-1", "RTMAX+1",
        ] {
            assert!(!valid_signal(bad), "{bad}");
        }
        let c = applied(&[ConfigEdit::StopSignal("sigquit".to_owned())]);
        assert_eq!(run(&c).stop_signal.as_deref(), Some("sigquit"));
        assert!(
            ConfigEdit::StopSignal("NOPE".to_owned())
                .apply(&mut ImageConfig::default())
                .is_err()
        );
    }

    #[test]
    fn other_fields_are_set() {
        let health = HealthConfig {
            test: Some(s(&["CMD-SHELL", "curl -f localhost"])),
            interval: Some(30_000_000_000),
            retries: Some(3),
            ..Default::default()
        };
        let c = applied(&[
            ConfigEdit::User("app:app".to_owned()),
            ConfigEdit::Healthcheck(health.clone()),
            ConfigEdit::OnBuild("RUN make".to_owned()),
            ConfigEdit::OnBuild("COPY . /src".to_owned()),
        ]);
        assert_eq!(run(&c).user.as_deref(), Some("app:app"));
        assert_eq!(run(&c).healthcheck, Some(health));
        assert_eq!(run(&c).on_build, Some(s(&["RUN make", "COPY . /src"])));
        assert!(
            ConfigEdit::Shell(Vec::new())
                .apply(&mut ImageConfig::default())
                .is_err()
        );
    }

    #[test]
    fn user_keeps_two_parts_as_kaniko() {
        for (written, stored) in [
            ("app", "app"),
            ("app:grp", "app:grp"),
            ("a:b:c", "a:b"),
            (":", ":"),
            ("", ""),
        ] {
            let c = applied(&[ConfigEdit::User(written.to_owned())]);
            assert_eq!(run(&c).user.as_deref(), Some(stored), "{written}");
        }
    }

    #[test]
    fn healthcheck_zeros_are_left_out() {
        let health = HealthConfig {
            test: Some(s(&["CMD", "true"])),
            interval: Some(0),
            timeout: Some(5),
            start_period: Some(0),
            start_interval: Some(0),
            retries: Some(0),
        };
        let c = applied(&[ConfigEdit::Healthcheck(health)]);
        assert_eq!(
            serde_json::to_value(run(&c)).unwrap(),
            json!({"Healthcheck": {"Test": ["CMD", "true"], "Timeout": 5}})
        );
    }

    #[test]
    fn empty_lists_are_refused() {
        for edit in [
            ConfigEdit::Env(Vec::new()),
            ConfigEdit::Label(Vec::new()),
            ConfigEdit::Expose(Vec::new()),
            ConfigEdit::Volume(Vec::new()),
            ConfigEdit::Shell(Vec::new()),
        ] {
            let mut c = ImageConfig::default();
            assert!(edit.apply(&mut c).is_err(), "{}", edit.instruction());
            assert_eq!(c, ImageConfig::default());
        }
    }

    #[test]
    fn layers_and_history() {
        let mut c = ImageConfig::default();
        let d = Digest::from_sha256_bytes(&[7; 32]);
        c.push_history(History {
            created_by: Some("ENV A=1".to_owned()),
            empty_layer: Some(false),
            ..Default::default()
        });
        c.push_layer(
            d.clone(),
            History {
                created_by: Some("RUN make".to_owned()),
                empty_layer: Some(true),
                ..Default::default()
            },
        );
        assert_eq!(c.rootfs.diff_ids, vec![d.clone()]);
        let history = c.history.as_ref().unwrap();
        assert_eq!(history[0].empty_layer, Some(true));
        assert_eq!(history[1].empty_layer, None);
        let bytes = to_canonical_json(&c).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text,
            format!(
                r#"{{"architecture":"","history":[{{"created_by":"ENV A=1","empty_layer":true}},{{"created_by":"RUN make"}}],"os":"","rootfs":{{"diff_ids":["{d}"],"type":"layers"}}}}"#
            )
        );
    }
}
