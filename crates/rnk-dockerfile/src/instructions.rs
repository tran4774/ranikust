// Derived from BuildKit, https://github.com/moby/buildkit,
// frontend/dockerfile/instructions/{parse,commands,support}.go at tag
// v0.34.0.
// Copyright The BuildKit Authors. Licensed under the Apache License, Version 2.0.
// Modified for ranikust: Rust port; one enum replaces the command types,
// lint rules and suggestions are left out.

//! The typed instruction set: BuildKit's `instructions.Parse`.

mod expand;
mod flags;
mod mount;

use std::time::Duration;

use crate::error::{Error, ErrorKind, Lines};
use crate::goutil::{
    equal_fold_ascii, parse_duration, parse_int32, to_lower, to_upper, trim_space,
};
use crate::parser::{
    Ast, Directive, Node, Warning, chomp_heredoc_content, parse_ast, parse_heredoc,
};
use flags::{FlagType, Flags};
pub use mount::{Device, Mount, MountType, Network, Security, Sharing};
pub(crate) use mount::{parse_device, parse_mount};

/// A parsed Dockerfile: BuildKit's `instructions.Parse` result plus the
/// parser's escape character and directives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dockerfile {
    /// The escape character, `\` or `` ` ``; variable expansion uses it too.
    pub escape: char,
    /// Parser directives (`syntax`, `escape`, `check`).
    pub directives: Vec<Directive>,
    /// `ARG` instructions before the first `FROM`.
    pub meta_args: Vec<Located<Vec<ArgDecl>>>,
    /// Build stages, in file order.
    pub stages: Vec<Stage>,
    /// Parser warnings (empty continuation lines).
    pub warnings: Vec<Warning>,
}

/// A value with its place in the Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<T> {
    /// The value.
    pub value: T,
    /// Lines the instruction spans, heredoc bodies included.
    pub lines: Lines,
    /// The instruction keyword as written (`RUN`, `run`).
    pub keyword: String,
    /// The instruction as written, continuation lines joined and trimmed:
    /// BuildKit's `String()`, used for history entries.
    pub code: String,
    /// Comment lines directly above the instruction.
    pub comments: Vec<String>,
}

/// One build stage: a `FROM` and the instructions up to the next one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// The name after `AS`, lower-cased; `None` for an unnamed stage.
    pub name: Option<String>,
    /// The base as written, not expanded: an image, a stage name or
    /// `scratch`.
    pub base: String,
    /// `--platform`, not expanded.
    pub platform: Option<String>,
    /// Lines of the `FROM` instruction.
    pub lines: Lines,
    /// The `FROM` instruction as written.
    pub code: String,
    /// Comment lines above the `FROM`.
    pub comments: Vec<String>,
    /// The stage's instructions.
    pub instructions: Vec<Located<Instruction>>,
}

/// A key and value of `ENV` or `LABEL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    /// The key, not yet expanded.
    pub key: String,
    /// The value, not yet expanded (quotes are still there).
    pub value: String,
    /// Written in the legacy `ENV key value` form, without `=`.
    pub no_delim: bool,
}

/// One name of an `ARG`, with its optional default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgDecl {
    /// The name.
    pub key: String,
    /// The default after `=`, not yet expanded; `None` without `=`.
    pub value: Option<String>,
}

/// A heredoc that `RUN` passes to the shell (BuildKit's
/// `ShellInlineFile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineFile {
    /// The delimiter word.
    pub name: String,
    /// The body as written; leading tabs are not stripped yet.
    pub data: String,
    /// `<<-`: strip leading tabs.
    pub chomp: bool,
}

/// The command of `RUN`, `CMD` or `ENTRYPOINT` (BuildKit's
/// `ShellDependantCmdLine`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CmdLine {
    /// Exec form: the arguments; shell form: one string, the joined line.
    /// Empty for `CMD []`, `ENTRYPOINT []` (which clears the entrypoint)
    /// and a bare `RUN`.
    pub args: Vec<String>,
    /// Shell form: run `args[0]` with the stage's shell.
    pub prepend_shell: bool,
    /// Heredocs of a `RUN`.
    pub files: Vec<InlineFile>,
}

/// A heredoc used as a `COPY`/`ADD` source (BuildKit's `SourceContent`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceContent {
    /// The file name: the delimiter word.
    pub path: String,
    /// The content, leading tabs already stripped for `<<-`.
    pub data: String,
    /// Expand variables in the content (the delimiter was not quoted).
    pub expand: bool,
}

/// `RUN`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    /// The command.
    pub cmd: CmdLine,
    /// `--mount` values as written.
    pub mount_specs: Vec<String>,
    /// The mounts. As in BuildKit before dispatch, only `from` is read at
    /// parse time (every mount is a bind mount until then);
    /// [`Located::expand`] or [`Run::parse_mounts`] parse every field.
    pub mounts: Vec<Mount>,
    /// `--network`.
    pub network: Network,
    /// `--security`.
    pub security: Security,
    /// `--device` values.
    pub devices: Vec<Device>,
    /// Names of the flags given, sorted.
    pub flags_used: Vec<String>,
}

impl Run {
    /// Parses every `--mount` value completely, passing each value through
    /// `expand` first (BuildKit's `setMountState` with an expander).
    ///
    /// # Errors
    ///
    /// The message of the first invalid mount or failed expansion.
    pub fn parse_mounts(
        &mut self,
        expand: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<(), String> {
        let mut mounts = Vec::with_capacity(self.mount_specs.len());
        for spec in &self.mount_specs {
            mounts.push(parse_mount(spec, Some(&mut *expand))?);
        }
        self.mounts = mounts;
        Ok(())
    }
}

/// `COPY`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Copy {
    /// Source paths (heredoc sources are in `contents`).
    pub sources: Vec<String>,
    /// Heredoc sources.
    pub contents: Vec<SourceContent>,
    /// Destination.
    pub dest: String,
    /// `--from`: a stage name or index, or an image; empty for the context.
    pub from: String,
    /// `--chown`.
    pub chown: String,
    /// `--chmod`.
    pub chmod: String,
    /// `--link`.
    pub link: bool,
    /// `--parents`.
    pub parents: bool,
    /// `--exclude` patterns.
    pub excludes: Vec<String>,
}

/// `ADD`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Add {
    /// Source paths or URLs (heredoc sources are in `contents`).
    pub sources: Vec<String>,
    /// Heredoc sources.
    pub contents: Vec<SourceContent>,
    /// Destination.
    pub dest: String,
    /// `--chown`.
    pub chown: String,
    /// `--chmod`.
    pub chmod: String,
    /// `--link`.
    pub link: bool,
    /// `--exclude` patterns.
    pub excludes: Vec<String>,
    /// `--keep-git-dir`, if given.
    pub keep_git_dir: Option<bool>,
    /// `--checksum`.
    pub checksum: String,
    /// `--unpack`, if given.
    pub unpack: Option<bool>,
}

/// `HEALTHCHECK` (Docker's `HealthcheckConfig`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Healthcheck {
    /// `["NONE"]`, `["CMD", args...]` or `["CMD-SHELL", command]`.
    pub test: Vec<String>,
    /// `--interval`; zero when unset.
    pub interval: Duration,
    /// `--timeout`; zero when unset.
    pub timeout: Duration,
    /// `--start-period`; zero when unset.
    pub start_period: Duration,
    /// `--start-interval`; zero when unset.
    pub start_interval: Duration,
    /// `--retries`; zero when unset.
    pub retries: u32,
}

/// A Dockerfile instruction other than `FROM`.
///
/// Values are as written: variables are expanded by [`Located::expand`]
/// (instruction fields) and the stage graph (`FROM`, `ARG`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    /// `RUN`.
    Run(Run),
    /// `COPY`.
    Copy(Copy),
    /// `ADD`.
    Add(Add),
    /// `ENV`.
    Env(Vec<KeyValue>),
    /// `ARG`.
    Arg(Vec<ArgDecl>),
    /// `LABEL`.
    Label(Vec<KeyValue>),
    /// `WORKDIR`.
    Workdir(String),
    /// `USER`.
    User(String),
    /// `EXPOSE`, ports sorted as BuildKit sorts them.
    Expose(Vec<String>),
    /// `VOLUME`.
    Volume(Vec<String>),
    /// `ENTRYPOINT`.
    Entrypoint(CmdLine),
    /// `CMD`.
    Cmd(CmdLine),
    /// `SHELL`.
    Shell(Vec<String>),
    /// `STOPSIGNAL`.
    StopSignal(String),
    /// `HEALTHCHECK`.
    Healthcheck(Healthcheck),
    /// `ONBUILD`: the trigger as text, with its heredocs appended, as
    /// BuildKit stores it in the image config.
    Onbuild(String),
    /// `MAINTAINER` (deprecated).
    Maintainer(String),
}

impl Instruction {
    /// The lower-case instruction name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Run(_) => "run",
            Self::Copy(_) => "copy",
            Self::Add(_) => "add",
            Self::Env(_) => "env",
            Self::Arg(_) => "arg",
            Self::Label(_) => "label",
            Self::Workdir(_) => "workdir",
            Self::User(_) => "user",
            Self::Expose(_) => "expose",
            Self::Volume(_) => "volume",
            Self::Entrypoint(_) => "entrypoint",
            Self::Cmd(_) => "cmd",
            Self::Shell(_) => "shell",
            Self::StopSignal(_) => "stopsignal",
            Self::Healthcheck(_) => "healthcheck",
            Self::Onbuild(_) => "onbuild",
            Self::Maintainer(_) => "maintainer",
        }
    }
}

/// Parses a Dockerfile into stages of typed instructions, as BuildKit's
/// `parser.Parse` followed by `instructions.Parse`.
///
/// Instructions deferred to later phases (`ONBUILD`, `ADD <url>`,
/// `--mount=type=bind`, `COPY --link/--parents/--exclude`…) are parsed
/// like any other; the builder decides what it supports.
///
/// # Errors
///
/// An [`Error`] with the lines it refers to: [`ErrorKind::Syntax`] from the
/// line parser, [`ErrorKind::Instruction`] for an unknown instruction, bad
/// arguments or flags, or an instruction before the first `FROM`.
pub fn parse(src: &str) -> Result<Dockerfile, Error> {
    let ast = parse_ast(src)?;
    from_ast(&ast)
}

/// Builds the typed Dockerfile from a raw syntax tree.
///
/// # Errors
///
/// As [`parse`], without the syntax errors.
pub fn from_ast(ast: &Ast) -> Result<Dockerfile, Error> {
    let mut meta_args = Vec::new();
    let mut stages: Vec<Stage> = Vec::new();
    for node in &ast.nodes {
        let err = |m: String| Error::at(ErrorKind::Instruction, node.lines, m);
        match parse_node(node).map_err(err)? {
            Parsed::Stage(stage) => stages.push(stage),
            Parsed::Instruction(located) => {
                if let Some(stage) = stages.last_mut() {
                    stage.instructions.push(located);
                } else if let Instruction::Arg(args) = located.value {
                    meta_args.push(Located {
                        value: args,
                        lines: located.lines,
                        keyword: located.keyword,
                        code: located.code,
                        comments: located.comments,
                    });
                } else {
                    return Err(err("no build stage in current context".to_owned()));
                }
            }
        }
    }
    Ok(Dockerfile {
        escape: ast.escape,
        directives: ast.directives.clone(),
        meta_args,
        stages,
        warnings: ast.warnings.clone(),
    })
}

/// Parses one instruction of a raw tree (BuildKit's `ParseCommand`), for
/// example an `ONBUILD` trigger. `FROM` is rejected.
///
/// # Errors
///
/// An [`ErrorKind::Instruction`] error at the node's lines.
pub fn parse_instruction(node: &Node) -> Result<Located<Instruction>, Error> {
    match parse_node(node) {
        Ok(Parsed::Instruction(i)) => Ok(i),
        Ok(Parsed::Stage(_)) => Err(Error::at(
            ErrorKind::Instruction,
            node.lines,
            "*instructions.Stage is not a command type",
        )),
        Err(m) => Err(Error::at(ErrorKind::Instruction, node.lines, m)),
    }
}

enum Parsed {
    Stage(Stage),
    Instruction(Located<Instruction>),
}

/// The arguments of a node as BuildKit's `parseRequest` sees them.
struct Request<'a> {
    node: &'a Node,
    args: Vec<String>,
}

impl Request<'_> {
    fn flags(&self, defs: &[(&'static str, FlagType, &str)]) -> Result<Flags, String> {
        let mut f = Flags::new(defs);
        f.parse(&self.node.flags)?;
        Ok(f)
    }

    fn no_flags(&self) -> Result<(), String> {
        self.flags(&[]).map(|_| ())
    }
}

fn at_least_one(cmd: &str) -> String {
    format!("{cmd} requires at least one argument")
}

fn exactly_one(cmd: &str) -> String {
    format!("{cmd} requires exactly one argument")
}

fn parse_node(node: &Node) -> Result<Parsed, String> {
    let req = Request {
        node,
        args: node.flat_args(),
    };
    let located = |value: Instruction| {
        Parsed::Instruction(Located {
            value,
            lines: node.lines,
            keyword: node.keyword.clone(),
            code: trim_space(&node.original).to_owned(),
            comments: node.comments.clone(),
        })
    };
    let i = match to_lower(&node.keyword).as_str() {
        "env" => {
            req.no_flags()?;
            Instruction::Env(parse_kvps(&req.args, "ENV")?)
        }
        "maintainer" => {
            let [m] = req.args.as_slice() else {
                return Err(exactly_one("MAINTAINER"));
            };
            req.no_flags()?;
            Instruction::Maintainer(m.clone())
        }
        "label" => {
            req.no_flags()?;
            Instruction::Label(parse_kvps(&req.args, "LABEL")?)
        }
        "add" => Instruction::Add(parse_add(&req)?),
        "copy" => Instruction::Copy(parse_copy(&req)?),
        "from" => return parse_from(&req).map(Parsed::Stage),
        "onbuild" => Instruction::Onbuild(parse_onbuild(&req)?),
        "workdir" => {
            let [p] = req.args.as_slice() else {
                return Err(exactly_one("WORKDIR"));
            };
            req.no_flags()?;
            Instruction::Workdir(p.clone())
        }
        "run" => Instruction::Run(parse_run(&req)?),
        "cmd" => {
            req.no_flags()?;
            Instruction::Cmd(shell_dependent(&req))
        }
        "healthcheck" => Instruction::Healthcheck(parse_healthcheck(&req)?),
        "entrypoint" => {
            req.no_flags()?;
            Instruction::Entrypoint(shell_dependent(&req))
        }
        "expose" => {
            if req.args.is_empty() {
                return Err(at_least_one("EXPOSE"));
            }
            req.no_flags()?;
            let mut ports = req.args.clone();
            ports.sort();
            Instruction::Expose(ports)
        }
        "user" => {
            let [u] = req.args.as_slice() else {
                return Err(exactly_one("USER"));
            };
            req.no_flags()?;
            Instruction::User(u.clone())
        }
        "volume" => {
            if req.args.is_empty() {
                return Err(at_least_one("VOLUME"));
            }
            req.no_flags()?;
            let mut volumes = Vec::with_capacity(req.args.len());
            for v in &req.args {
                let v = trim_space(v);
                if v.is_empty() {
                    return Err("VOLUME specified can not be an empty string".to_owned());
                }
                volumes.push(v.to_owned());
            }
            Instruction::Volume(volumes)
        }
        "stopsignal" => {
            // BuildKit does not parse STOPSIGNAL's flags: they are ignored.
            let [s] = req.args.as_slice() else {
                return Err(exactly_one("STOPSIGNAL"));
            };
            Instruction::StopSignal(s.clone())
        }
        "arg" => Instruction::Arg(parse_arg(&req)?),
        "shell" => {
            req.no_flags()?;
            let args = json_args(&req.args, node.json);
            if args.is_empty() {
                return Err(at_least_one("SHELL"));
            }
            if !node.json {
                return Err("SHELL requires the arguments to be in JSON form".to_owned());
            }
            Instruction::Shell(args)
        }
        _ => return Err(format!("unknown instruction: {}", node.keyword)),
    };
    Ok(located(i))
}

/// BuildKit's `parseKvps`: `ENV`/`LABEL` arguments come in triples.
fn parse_kvps(args: &[String], cmd: &str) -> Result<Vec<KeyValue>, String> {
    if args.is_empty() {
        return Err(at_least_one(cmd));
    }
    let (triples, rest) = args.as_chunks::<3>();
    if !rest.is_empty() {
        return Err(format!("Bad input to {cmd}, too many arguments"));
    }
    triples
        .iter()
        .map(|[key, value, delim]| {
            if key.is_empty() {
                return Err(format!("{cmd} names can not be blank"));
            }
            Ok(KeyValue {
                key: key.clone(),
                value: value.clone(),
                no_delim: delim.is_empty(),
            })
        })
        .collect()
}

/// BuildKit's `handleJSONArgs`.
fn json_args(args: &[String], json: bool) -> Vec<String> {
    if args.is_empty() {
        Vec::new()
    } else if json {
        args.to_vec()
    } else {
        vec![args.join(" ")]
    }
}

/// BuildKit's `parseShellDependentCommand`.
fn shell_dependent(req: &Request<'_>) -> CmdLine {
    CmdLine {
        args: json_args(&req.args, req.node.json),
        prepend_shell: !req.node.json,
        files: req
            .node
            .heredocs
            .iter()
            .map(|h| InlineFile {
                name: h.name.clone(),
                data: h.content.clone(),
                chomp: h.chomp,
            })
            .collect(),
    }
}

fn parse_run(req: &Request<'_>) -> Result<Run, String> {
    let f = req.flags(&[
        ("device", FlagType::Strings, ""),
        ("mount", FlagType::Strings, ""),
        ("network", FlagType::String, "default"),
        ("security", FlagType::String, "sandbox"),
    ])?;
    let cmd = shell_dependent(req);
    // The post-parse hooks, in BuildKit's order.
    let devices = f
        .strings("device")
        .iter()
        .map(|d| parse_device(d))
        .collect::<Result<Vec<_>, _>>()?;
    let mount_specs = f.strings("mount");
    let mounts = mount_specs
        .iter()
        .map(|m| parse_mount(m, None))
        .collect::<Result<Vec<_>, _>>()?;
    let network = f.string("network");
    let network =
        Network::parse(&network).ok_or_else(|| format!("invalid network mode \"{network}\""))?;
    let security = f.string("security");
    let security = Security::parse(&security)
        .ok_or_else(|| format!("security \"{security}\" is not valid"))?;
    Ok(Run {
        cmd,
        mount_specs,
        mounts,
        network,
        security,
        devices,
        flags_used: f.used(),
    })
}

/// The heredoc a `COPY`/`ADD` argument names, if it is one.
fn arg_heredoc(arg: &str) -> Option<crate::parser::Heredoc> {
    // BuildKit's MustParseHeredoc: a lexing error means "not a heredoc".
    parse_heredoc(arg).ok().flatten()
}

/// BuildKit's `parseSourcesAndDest`.
fn sources_and_dest(
    req: &Request<'_>,
    cmd: &str,
) -> Result<(Vec<String>, Vec<SourceContent>, String), String> {
    let Some((dest, srcs)) = req.args.split_last() else {
        return Err(format!(
            "{cmd} requires at least two arguments, but only one was provided. Destination could not be determined"
        ));
    };
    if arg_heredoc(dest).is_some() {
        return Err(format!("{cmd} cannot accept a heredoc as a destination"));
    }
    let mut sources = Vec::new();
    let mut contents = Vec::new();
    for src in srcs {
        if let Some(h) = arg_heredoc(src) {
            // The last heredoc of that name wins, as in BuildKit's map.
            let content = req
                .node
                .heredocs
                .iter()
                .rev()
                .find(|d| d.name == h.name)
                .map(|d| d.content.clone())
                .unwrap_or_default();
            let data = if h.chomp {
                chomp_heredoc_content(&content)
            } else {
                content
            };
            contents.push(SourceContent {
                data,
                path: h.name,
                expand: h.expand,
            });
        } else {
            sources.push(src.clone());
        }
    }
    Ok((sources, contents, dest.clone()))
}

fn parse_add(req: &Request<'_>) -> Result<Add, String> {
    if req.args.len() < 2 {
        return Err("ADD requires at least two arguments, but only one was provided. Destination could not be determined".to_owned());
    }
    let f = req.flags(&[
        ("chown", FlagType::String, ""),
        ("chmod", FlagType::String, ""),
        ("link", FlagType::Bool, "false"),
        ("keep-git-dir", FlagType::Bool, "false"),
        ("checksum", FlagType::String, ""),
        ("unpack", FlagType::Bool, "false"),
        ("exclude", FlagType::Strings, ""),
    ])?;
    let (sources, contents, dest) = sources_and_dest(req, "ADD")?;
    Ok(Add {
        sources,
        contents,
        dest,
        chown: f.string("chown"),
        chmod: f.string("chmod"),
        link: f.bool("link"),
        excludes: f.strings("exclude"),
        keep_git_dir: f.opt_bool("keep-git-dir"),
        checksum: f.string("checksum"),
        unpack: f.opt_bool("unpack"),
    })
}

fn parse_copy(req: &Request<'_>) -> Result<Copy, String> {
    if req.args.len() < 2 {
        return Err("COPY requires at least two arguments, but only one was provided. Destination could not be determined".to_owned());
    }
    let f = req.flags(&[
        ("chown", FlagType::String, ""),
        ("from", FlagType::String, ""),
        ("chmod", FlagType::String, ""),
        ("link", FlagType::Bool, "false"),
        ("exclude", FlagType::Strings, ""),
        ("parents", FlagType::Bool, "false"),
    ])?;
    let (sources, contents, dest) = sources_and_dest(req, "COPY")?;
    Ok(Copy {
        sources,
        contents,
        dest,
        from: f.string("from"),
        chown: f.string("chown"),
        chmod: f.string("chmod"),
        link: f.bool("link"),
        parents: f.bool("parents"),
        excludes: f.strings("exclude"),
    })
}

/// BuildKit's `validStageName`: `^[a-z][a-z0-9-_.]*$`.
fn valid_stage_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn parse_from(req: &Request<'_>) -> Result<Stage, String> {
    let name = match req.args.as_slice() {
        [_, as_kw, name] if equal_fold_ascii(as_kw, "as") => {
            let lower = to_lower(name);
            if !valid_stage_name(&lower) {
                return Err(format!(
                    "invalid name for build stage: \"{name}\", name can't start with a number or contain symbols"
                ));
            }
            Some(lower)
        }
        [_] => None,
        _ => return Err("FROM requires either one or three arguments".to_owned()),
    };
    let f = req.flags(&[("platform", FlagType::String, "")])?;
    let platform = f.string("platform");
    Ok(Stage {
        name,
        base: req.args[0].clone(),
        platform: (!platform.is_empty()).then_some(platform),
        lines: req.node.lines,
        code: trim_space(&req.node.original).to_owned(),
        comments: req.node.comments.clone(),
        instructions: Vec::new(),
    })
}

/// `(?i)^\s*ONBUILD\s*` removed from the start of `s`.
fn strip_onbuild_keyword(s: &str) -> &str {
    let is_re_space = |c: char| matches!(c, ' ' | '\t' | '\n' | '\u{c}' | '\r');
    let t = s.trim_start_matches(is_re_space);
    match t.get(..7) {
        Some(kw) if kw.eq_ignore_ascii_case("onbuild") => t[7..].trim_start_matches(is_re_space),
        _ => s,
    }
}

fn parse_onbuild(req: &Request<'_>) -> Result<String, String> {
    let Some(first) = req.args.first() else {
        return Err(at_least_one("ONBUILD"));
    };
    req.no_flags()?;
    let trigger = to_upper(trim_space(first));
    match trigger.as_str() {
        "ONBUILD" => {
            return Err("Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed".to_owned());
        }
        "MAINTAINER" | "FROM" => {
            return Err(format!("{trigger} isn't allowed as an ONBUILD trigger"));
        }
        _ => {}
    }
    let mut expression = strip_onbuild_keyword(&req.node.original).to_owned();
    for h in &req.node.heredocs {
        expression.push('\n');
        expression.push_str(&h.content);
        expression.push_str(&h.name);
    }
    Ok(expression)
}

/// BuildKit's `parseOptInterval`: zero when empty, else at least 1ms.
fn parse_interval(f: &Flags, name: &str) -> Result<Duration, String> {
    let s = f.string(name);
    if s.is_empty() {
        return Ok(Duration::ZERO);
    }
    let nanos = parse_duration(&s)?;
    if nanos == 0 {
        return Ok(Duration::ZERO);
    }
    let Ok(nanos) = u64::try_from(nanos) else {
        return Err(format!("Interval \"{name}\" cannot be less than 1ms"));
    };
    if nanos < 1_000_000 {
        return Err(format!("Interval \"{name}\" cannot be less than 1ms"));
    }
    Ok(Duration::from_nanos(nanos))
}

fn parse_healthcheck(req: &Request<'_>) -> Result<Healthcheck, String> {
    let Some((first, args)) = req.args.split_first() else {
        return Err(at_least_one("HEALTHCHECK"));
    };
    let typ = to_upper(first);
    if typ == "NONE" {
        // BuildKit does not parse the flags of HEALTHCHECK NONE.
        if !args.is_empty() {
            return Err("HEALTHCHECK NONE takes no arguments".to_owned());
        }
        return Ok(Healthcheck {
            test: vec![typ],
            ..Healthcheck::default()
        });
    }
    let f = req.flags(&[
        ("interval", FlagType::String, ""),
        ("timeout", FlagType::String, ""),
        ("start-period", FlagType::String, ""),
        ("start-interval", FlagType::String, ""),
        ("retries", FlagType::String, ""),
    ])?;
    if typ != "CMD" {
        return Err(format!("Unknown type \"{typ}\" in HEALTHCHECK (try CMD)"));
    }
    let cmd = json_args(args, req.node.json);
    if cmd.is_empty() {
        return Err("Missing command after HEALTHCHECK CMD".to_owned());
    }
    let mut test = vec![if req.node.json {
        typ
    } else {
        "CMD-SHELL".to_owned()
    }];
    test.extend(cmd);
    let interval = parse_interval(&f, "interval")?;
    let timeout = parse_interval(&f, "timeout")?;
    let start_period = parse_interval(&f, "start-period")?;
    let start_interval = parse_interval(&f, "start-interval")?;
    let retries = f.string("retries");
    let retries = if retries.is_empty() {
        0
    } else {
        let r = parse_int32(&retries)
            .ok_or_else(|| format!("strconv.ParseInt: parsing \"{retries}\": invalid syntax"))?;
        u32::try_from(r).map_err(|_| format!("--retries cannot be negative ({r})"))?
    };
    Ok(Healthcheck {
        test,
        interval,
        timeout,
        start_period,
        start_interval,
        retries,
    })
}

fn parse_arg(req: &Request<'_>) -> Result<Vec<ArgDecl>, String> {
    // BuildKit does not parse ARG's flags: they are ignored.
    if req.args.is_empty() {
        return Err(at_least_one("ARG"));
    }
    req.args
        .iter()
        .map(|arg| match arg.split_once('=') {
            Some(("", _)) => Err("ARG names can not be blank".to_owned()),
            Some((k, v)) => Ok(ArgDecl {
                key: k.to_owned(),
                value: Some(v.to_owned()),
            }),
            None => Ok(ArgDecl {
                key: arg.clone(),
                value: None,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests;
