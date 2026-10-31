//! What ranikust cannot close itself in kaniko mode on `/` (ADR-0051), and
//! warns about: a `RUN` there shares ranikust's PID namespace, so
//!
//! - `/proc/<pid>/cmdline` of every process, ranikust's included, is
//!   readable whatever `PR_SET_DUMPABLE` says: a `--build-arg` value on the
//!   command line is readable by every `RUN`;
//! - another process of the container that holds a credential in its
//!   environment (the CI shell that started ranikust) exposes it through its
//!   own `/proc/<pid>/environ`.

use std::fs;

use crate::config::BuildArg;

/// The credential variables ranikust reads.
pub(crate) const CREDENTIAL_VARS: [&str; 1] = ["DOCKER_AUTH_CONFIG"];

/// Words that make an argument name look like a secret.
const SECRET_WORDS: [&str; 8] = [
    "TOKEN",
    "PASSWORD",
    "PASSWD",
    "SECRET",
    "KEY",
    "CREDENTIAL",
    "AUTH",
    "PRIVATE",
];

/// Whether a build argument's name looks like a secret's.
pub(crate) fn looks_secret(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    SECRET_WORDS.iter().any(|w| upper.contains(w))
}

/// Warns for each `--build-arg NAME=value` whose name looks like a
/// secret's: its value is on ranikust's command line. `--build-arg NAME`
/// alone, which takes the value from the environment, is not on it.
pub(crate) fn warn_build_args(args: &[BuildArg]) {
    for arg in args {
        if arg.value.is_some() && looks_secret(&arg.name) {
            tracing::warn!(
                "--build-arg {}=…: the value is on ranikust's command line, which every RUN can \
                 read through /proc/<pid>/cmdline in kaniko mode on /; pass a secret with \
                 --secret, or as `--build-arg {}` with the value in the environment and \
                 ranikust started as the container's PID 1 (exec it), else the shell that \
                 started it holds the value where a RUN reads it",
                arg.name,
                arg.name
            );
        }
    }
}

/// Every variable of ranikust's environment that holds a credential or a
/// secret: [`CREDENTIAL_VARS`], the variable of each `--secret` read from
/// the environment (`env=`, or the id itself when set), and each
/// `--build-arg NAME` given without a value.
pub(crate) fn secret_variables(config: &crate::config::BuildConfig) -> Vec<String> {
    let mut names: Vec<String> = CREDENTIAL_VARS.iter().map(|&n| n.to_owned()).collect();
    for (_, secret) in &config.secrets {
        if let rnk_builder::Secret::Env(var) = secret {
            names.push(var.to_string_lossy().into_owned());
        }
    }
    for arg in &config.build_args {
        if arg.value.is_none() {
            names.push(arg.name.clone());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The names among `names` set to a non-empty value in an `environ` file's
/// contents (`NAME=value` entries separated by NUL bytes).
pub(crate) fn held<'a>(environ: &[u8], names: &[&'a str]) -> Vec<&'a str> {
    names
        .iter()
        .copied()
        .filter(|name| {
            environ.split(|&b| b == 0).any(|entry| {
                entry
                    .strip_prefix(name.as_bytes())
                    .and_then(|rest| rest.strip_prefix(b"="))
                    .is_some_and(|value| !value.is_empty())
            })
        })
        .collect()
}

/// Warns for every other process of the PID namespace whose environment
/// holds one of `variables` ([`secret_variables`]): a `RUN` reads it
/// through that process's `/proc/<pid>/environ`. Processes ranikust cannot
/// read (not dumpable, another user) are skipped: a `RUN` running as root
/// cannot read them either without `CAP_SYS_PTRACE`.
pub(crate) fn warn_credential_holders(variables: &[String]) {
    let names: Vec<&str> = variables.iter().map(String::as_str).collect();
    let me = std::process::id().to_string();
    let Ok(dir) = fs::read_dir("/proc") else {
        return;
    };
    for entry in dir.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(environ) = fs::read(entry.path().join("environ")) else {
            continue;
        };
        let found = held(&environ, &names);
        if found.is_empty() {
            continue;
        }
        let comm = fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        tracing::warn!(
            "process {pid} ({}) holds {} in its environment, which every RUN can read through \
             /proc/{pid}/environ in kaniko mode on /; start ranikust as the container's PID 1 \
             (exec it) so that no other process holds the credentials",
            comm.trim(),
            found.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_looking_names() {
        for n in [
            "NPM_TOKEN",
            "db_password",
            "SSH_PRIVATE_KEY",
            "AUTH",
            "api-key",
        ] {
            assert!(looks_secret(n), "{n}");
        }
        for n in ["VERSION", "NODE_ENV", "TARGETPLATFORM"] {
            assert!(!looks_secret(n), "{n}");
        }
    }

    #[test]
    fn held_variables_are_found_by_exact_name() {
        let env = b"PATH=/bin\0DOCKER_AUTH_CONFIG={\"auths\":{}}\0X_DOCKER_AUTH_CONFIG=1\0";
        assert_eq!(held(env, &CREDENTIAL_VARS), ["DOCKER_AUTH_CONFIG"]);
        assert!(held(b"DOCKER_AUTH_CONFIG=\0", &CREDENTIAL_VARS).is_empty());
        assert!(held(b"DOCKER_AUTH_CONFIGX=a\0", &CREDENTIAL_VARS).is_empty());
    }

    #[test]
    fn secret_variables_cover_env_secrets_and_valueless_build_args() {
        let argv = [
            "ranikust",
            "--no-push",
            "--secret",
            "id=npm,env=NPM_SECRET_VAR",
            "--secret",
            "id=file_one,src=/ranikust/s",
            "--build-arg",
            "GH_TOKEN",
            "--build-arg",
            "VERSION=1",
        ];
        let crate::cli::Command::Build(args) = crate::cli::parse(argv).unwrap().cli.into_command()
        else {
            panic!("expected build");
        };
        let config = crate::config::BuildConfig::from_args(&args).unwrap();
        assert_eq!(
            secret_variables(&config),
            ["DOCKER_AUTH_CONFIG", "GH_TOKEN", "NPM_SECRET_VAR"]
        );
    }
}
