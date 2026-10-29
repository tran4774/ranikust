// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/dockerfile2llb/convert.go and platform.go at tag
// v0.34.0 (defaultArgs, buildMetaArgs, dispatchArg, dispatchEnv, addEnv).
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: the variable bookkeeping of the dispatcher, without
// LLB.

//! Variables: the built-in and global `ARG`s, and the environment of a stage
//! as its instructions run.

use std::collections::BTreeMap;

use crate::error::{Error, ErrorKind};
use crate::instructions::{ArgDecl, Instruction, Located};
use crate::shell::{Env, Lexer};

/// `PATH` that BuildKit adds when an image sets none
/// (`system.DefaultPathEnv("linux")`).
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// `ARG`s that are never added to the environment.
const NON_ENV_ARGS: [&str; 2] = ["BUILDKIT_SBOM_SCAN_CONTEXT", "BUILDKIT_SBOM_SCAN_STAGE"];

/// An ordered list of variables: a later `set` of the same name replaces the
/// value in place, as Docker's image config `Env` does.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EnvList {
    vars: Vec<(String, String)>,
}

impl EnvList {
    /// An empty list.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `KEY=VALUE` entries, as in an image config; an entry without
    /// `=` sets the empty string.
    #[must_use]
    pub fn from_entries<S: AsRef<str>>(entries: &[S]) -> Self {
        let mut l = Self::new();
        for e in entries {
            let (k, v) = e.as_ref().split_once('=').unwrap_or((e.as_ref(), ""));
            l.set(k, v);
        }
        l
    }

    /// The value of `name`.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// Sets `name`, replacing an earlier value in place.
    pub fn set(&mut self, name: &str, value: &str) {
        match self.vars.iter_mut().find(|(k, _)| k == name) {
            Some((_, v)) => value.clone_into(v),
            None => self.vars.push((name.to_owned(), value.to_owned())),
        }
    }

    /// The variables in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.vars.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// `KEY=VALUE` entries in order, as an image config stores them.
    #[must_use]
    pub fn to_entries(&self) -> Vec<String> {
        self.vars.iter().map(|(k, v)| format!("{k}={v}")).collect()
    }

    /// Number of variables.
    #[must_use]
    pub fn len(&self) -> usize {
        self.vars.len()
    }

    /// No variables.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }
}

impl Env for EnvList {
    fn get(&self, name: &str) -> Option<&str> {
        EnvList::get(self, name)
    }
}

/// A platform (`os/arch[/variant]`), as the built-in `ARG`s describe it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Platform {
    /// `linux`.
    pub os: String,
    /// `amd64`, `arm64`…
    pub architecture: String,
    /// `v7`, `v8`…; empty when none.
    pub variant: String,
    /// OS version (Windows); empty on Linux.
    pub os_version: String,
}

impl Platform {
    /// A platform without variant or OS version.
    #[must_use]
    pub fn new(os: &str, architecture: &str) -> Self {
        Self {
            os: os.to_owned(),
            architecture: architecture.to_owned(),
            ..Self::default()
        }
    }

    /// containerd's `platforms.Format`: `os/arch[/variant]`, `unknown`
    /// without an OS.
    #[must_use]
    pub fn format(&self) -> String {
        if self.os.is_empty() {
            return "unknown".to_owned();
        }
        let mut s = format!("{}/{}", self.os, self.architecture);
        if !self.variant.is_empty() {
            s.push('/');
            s.push_str(&self.variant);
        }
        s
    }

    /// containerd's `platforms.FormatAll`: as [`Platform::format`], with
    /// the OS version in parentheses after the OS when there is one.
    #[must_use]
    pub fn format_all(&self) -> String {
        if self.os.is_empty() || self.os_version.is_empty() {
            return self.format();
        }
        let mut p = self.clone();
        p.os = format!("{}({})", self.os, self.os_version);
        p.format()
    }
}

/// BuildKit's `defaultArgs`: the built-in `ARG`s, overridden by build
/// arguments of the same name. `target` is the target stage's name
/// (`default` when empty).
pub(crate) fn default_args(
    build: &Platform,
    target: &Platform,
    build_args: &BTreeMap<String, String>,
    target_stage: &str,
) -> EnvList {
    let target_stage = if target_stage.is_empty() {
        "default"
    } else {
        target_stage
    };
    let defaults = [
        ("BUILDPLATFORM", build.format()),
        ("BUILDOS", build.os.clone()),
        ("BUILDOSVERSION", build.os_version.clone()),
        ("BUILDARCH", build.architecture.clone()),
        ("BUILDVARIANT", build.variant.clone()),
        ("TARGETPLATFORM", target.format_all()),
        ("TARGETOS", target.os.clone()),
        ("TARGETOSVERSION", target.os_version.clone()),
        ("TARGETARCH", target.architecture.clone()),
        ("TARGETVARIANT", target.variant.clone()),
        ("TARGETSTAGE", target_stage.to_owned()),
    ];
    let mut env = EnvList::new();
    for (k, v) in defaults {
        env.set(k, build_args.get(k).map_or(v.as_str(), String::as_str));
    }
    env
}

/// BuildKit's `buildMetaArgs`: the `ARG`s before the first `FROM`, each set
/// from `--build-arg`, else from its default expanded with the variables
/// before it, else left unset.
pub(crate) fn build_meta_args(
    mut args: EnvList,
    lex: &Lexer,
    meta_args: &[Located<Vec<ArgDecl>>],
    build_args: &BTreeMap<String, String>,
) -> Result<EnvList, Error> {
    for cmd in meta_args {
        for kp in &cmd.value {
            let value = match (build_args.get(&kp.key), &kp.value) {
                (Some(v), _) => Some(v.clone()),
                (None, Some(default)) => Some(
                    lex.process_word(default, &args)
                        .map_err(|e| Error::at(ErrorKind::Expansion, cmd.lines, e.to_string()))?,
                ),
                (None, None) => None,
            };
            if let Some(v) = value {
                args.set(&kp.key, &v);
            }
        }
    }
    Ok(args)
}

/// The variables of one stage while its instructions run: BuildKit's
/// dispatch state, without the build itself.
///
/// The caller creates it from the base image's config `Env`
/// ([`StageScope::from_image`]) or clones the final scope of the base stage
/// (`FROM <stage>`), then passes every instruction through
/// [`StageScope::apply`] in order, `ONBUILD` triggers first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageScope {
    lexer: Lexer,
    global_args: EnvList,
    build_args: BTreeMap<String, String>,
    env: EnvList,
    config_env: EnvList,
    args: Vec<ArgDecl>,
}

impl StageScope {
    /// The scope at the start of a stage whose base is an image (or
    /// `scratch`, with no entries). As BuildKit does, `PATH` is added to the
    /// image config when the image sets none.
    #[must_use]
    pub fn from_image<S: AsRef<str>>(
        lexer: Lexer,
        global_args: &EnvList,
        build_args: &BTreeMap<String, String>,
        image_env: &[S],
    ) -> Self {
        let mut config_env = EnvList::from_entries(image_env);
        if config_env.get("PATH").is_none() {
            config_env.set("PATH", DEFAULT_PATH);
        }
        Self {
            lexer,
            global_args: global_args.clone(),
            build_args: build_args.clone(),
            env: config_env.clone(),
            config_env,
            args: Vec::new(),
        }
    }

    /// The environment instructions see and `RUN` gets: the image's,
    /// `ENV`s and `ARG`s with a value.
    #[must_use]
    pub fn env(&self) -> &EnvList {
        &self.env
    }

    /// The image config's `Env`: the image's and `ENV`s, not `ARG`s.
    #[must_use]
    pub fn config_env(&self) -> &EnvList {
        &self.config_env
    }

    /// `ARG`s declared so far, with their values (`None` when unset).
    #[must_use]
    pub fn args(&self) -> &[ArgDecl] {
        &self.args
    }

    /// Expands one instruction for this point of the stage and records its
    /// effect on the variables (`ENV`, `ARG`). Returns the expanded
    /// instruction; an `ARG` comes back with its resolved values.
    ///
    /// # Errors
    ///
    /// As [`Located::expand`]; an `ARG` default that cannot be expanded is
    /// an [`ErrorKind::Expansion`] error.
    pub fn apply(
        &mut self,
        instruction: &Located<Instruction>,
    ) -> Result<Located<Instruction>, Error> {
        if let Instruction::Arg(decls) = &instruction.value {
            let mut resolved = Vec::with_capacity(decls.len());
            for a in decls {
                resolved.push(self.declare(a, instruction)?);
            }
            return Ok(Located {
                value: Instruction::Arg(resolved),
                lines: instruction.lines,
                keyword: instruction.keyword.clone(),
                code: instruction.code.clone(),
                comments: instruction.comments.clone(),
            });
        }
        let mut expanded = instruction.clone();
        expanded.expand(&self.lexer, &self.env)?;
        if let Instruction::Env(kvs) = &expanded.value {
            for kv in kvs {
                self.env.set(&kv.key, &kv.value);
                self.config_env.set(&kv.key, &kv.value);
            }
        }
        Ok(expanded)
    }

    /// BuildKit's `dispatchArg` for one name: `--build-arg` wins, then the
    /// default expanded with the current environment, then a global `ARG`
    /// of the same name when there is no default.
    fn declare(&mut self, a: &ArgDecl, at: &Located<Instruction>) -> Result<ArgDecl, Error> {
        let value = match (self.build_args.get(&a.key), &a.value) {
            (Some(v), _) => Some(v.clone()),
            (None, Some(default)) => Some(
                self.lexer
                    .process_word(default, &self.env)
                    .map_err(|e| Error::at(ErrorKind::Expansion, at.lines, e.to_string()))?,
            ),
            (None, None) => self.global_args.get(&a.key).map(str::to_owned),
        };
        if let Some(v) = &value
            && !NON_ENV_ARGS.contains(&a.key.as_str())
        {
            self.env.set(&a.key, v);
        }
        let decl = ArgDecl {
            key: a.key.clone(),
            value,
        };
        self.args.push(decl.clone());
        Ok(decl)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn env_list_replaces_in_place() {
        let mut l = EnvList::from_entries(&["A=1", "B", "C=x=y"]);
        l.set("A", "2");
        l.set("D", "4");
        assert_eq!(l.to_entries(), ["A=2", "B=", "C=x=y", "D=4"]);
        assert_eq!(l.get("B"), Some(""));
        assert_eq!(l.get("Z"), None);
    }

    #[test]
    fn builtin_args() {
        let mut build = Platform::new("linux", "amd64");
        let mut target = Platform::new("linux", "arm");
        target.variant = "v7".to_owned();
        let env = default_args(&build, &target, &map(&[("TARGETOS", "x")]), "");
        assert_eq!(env.get("BUILDPLATFORM"), Some("linux/amd64"));
        assert_eq!(env.get("TARGETPLATFORM"), Some("linux/arm/v7"));
        assert_eq!(env.get("TARGETVARIANT"), Some("v7"));
        assert_eq!(env.get("TARGETOS"), Some("x"));
        assert_eq!(env.get("TARGETSTAGE"), Some("default"));
        assert_eq!(env.get("BUILDVARIANT"), Some(""));
        build.os_version = "10.0".to_owned();
        assert_eq!(build.format_all(), "linux(10.0)/amd64");
        assert_eq!(Platform::default().format(), "unknown");
    }

    #[test]
    fn meta_args_follow_build_args_then_defaults() {
        let lex = Lexer::new('\\');
        let df = crate::parse("ARG A=1 B=${A}2 C D=$C\nARG A=x$A\nFROM a\n").unwrap();
        let global =
            build_meta_args(EnvList::new(), &lex, &df.meta_args, &map(&[("C", "c")])).unwrap();
        assert_eq!(global.to_entries(), ["A=x1", "B=12", "C=c", "D=c"]);
        let global = build_meta_args(EnvList::new(), &lex, &df.meta_args, &map(&[])).unwrap();
        assert_eq!(global.get("C"), None);
        assert_eq!(global.get("D"), Some(""));
    }

    #[test]
    fn stage_scope_tracks_env_and_args() {
        let lex = Lexer::new('\\');
        let df = crate::parse(
            "FROM a\nARG G\nARG L=$G-l X\nENV E=$L P=$PATH\nENV Q=$E\nARG B=default\nWORKDIR /$E/$B/$X\n",
        )
        .unwrap();
        let global = EnvList::from_entries(&["G=global", "X=x"]);
        let mut s = StageScope::from_image(lex, &global, &map(&[("B", "built")]), &["PATH=/bin"]);
        let mut out = Vec::new();
        for i in &df.stages[0].instructions {
            out.push(s.apply(i).unwrap());
        }
        assert_eq!(
            s.env().to_entries(),
            [
                "PATH=/bin",
                "G=global",
                "L=global-l",
                "X=x",
                "E=global-l",
                "P=/bin",
                "Q=global-l",
                "B=built"
            ]
        );
        assert_eq!(
            s.config_env().to_entries(),
            ["PATH=/bin", "E=global-l", "P=/bin", "Q=global-l"]
        );
        assert_eq!(
            out[5].value,
            Instruction::Workdir("/global-l/built/x".to_owned())
        );
        assert_eq!(s.args().len(), 4);
    }

    #[test]
    fn undeclared_global_args_are_invisible_in_stages() {
        let lex = Lexer::new('\\');
        let df = crate::parse("FROM a\nWORKDIR /$G\nARG U\nUSER ${U-unset}\n").unwrap();
        let global = EnvList::from_entries(&["G=global"]);
        let mut s = StageScope::from_image(lex, &global, &BTreeMap::new(), &Vec::<String>::new());
        let w = s.apply(&df.stages[0].instructions[0]).unwrap();
        assert_eq!(w.value, Instruction::Workdir("/".to_owned()));
        s.apply(&df.stages[0].instructions[1]).unwrap();
        let u = s.apply(&df.stages[0].instructions[2]).unwrap();
        assert_eq!(u.value, Instruction::User("unset".to_owned()));
        assert_eq!(s.config_env().get("PATH"), Some(DEFAULT_PATH));
    }

    #[test]
    fn sbom_args_stay_out_of_the_environment() {
        let lex = Lexer::new('\\');
        let df = crate::parse("FROM a\nARG BUILDKIT_SBOM_SCAN_STAGE=true\n").unwrap();
        let mut s = StageScope::from_image(lex, &EnvList::new(), &BTreeMap::new(), &["PATH=/"]);
        s.apply(&df.stages[0].instructions[0]).unwrap();
        assert_eq!(s.env().len(), 1);
        assert_eq!(s.args()[0].value.as_deref(), Some("true"));
    }

    #[test]
    fn arg_default_errors_carry_lines() {
        let lex = Lexer::new('\\');
        let df = crate::parse("FROM a\n\nARG A=${X:?missing}\n").unwrap();
        let mut s = StageScope::from_image(lex, &EnvList::new(), &BTreeMap::new(), &["PATH=/"]);
        let e = s.apply(&df.stages[0].instructions[0]).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Expansion);
        assert_eq!(e.lines().map(|l| l.start), Some(3));
        let e = build_meta_args(
            EnvList::new(),
            &lex,
            &crate::parse("ARG A=${X:?no}\nFROM a\n").unwrap().meta_args,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert_eq!(e.lines().map(|l| l.start), Some(1));
    }
}
