use std::collections::BTreeMap;

use super::*;
use crate::shell::{Lexer, NoEnv};

fn df(src: &str) -> Dockerfile {
    parse(src).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn err(src: &str) -> Error {
    parse(src).expect_err(src)
}

fn only(src: &str) -> Located<Instruction> {
    let d = df(&format!("FROM a\n{src}\n"));
    d.stages[0].instructions[0].clone()
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

#[test]
fn stages_and_meta_args() {
    let d =
        df("ARG A=1 B\nFROM --platform=$P x AS Build\nRUN a\nFROM build as two\nFROM scratch\n");
    assert_eq!(d.meta_args.len(), 1);
    assert_eq!(
        d.meta_args[0].value,
        [
            ArgDecl {
                key: "A".to_owned(),
                value: Some("1".to_owned())
            },
            ArgDecl {
                key: "B".to_owned(),
                value: None
            }
        ]
    );
    assert_eq!(d.stages.len(), 3);
    let s = &d.stages[0];
    assert_eq!(s.name.as_deref(), Some("build"));
    assert_eq!(s.base, "x");
    assert_eq!(s.platform.as_deref(), Some("$P"));
    assert_eq!(s.code, "FROM --platform=$P x AS Build");
    assert_eq!(s.instructions.len(), 1);
    assert_eq!(s.instructions[0].lines, Lines::single(3));
    assert_eq!(d.stages[1].name.as_deref(), Some("two"));
    assert_eq!(d.stages[2].name, None);
}

#[test]
fn from_errors() {
    assert!(
        err("FROM a AS 1x\n")
            .message()
            .starts_with("invalid name for build stage")
    );
    assert!(
        err("FROM a AS b-\u{e9}\n")
            .message()
            .starts_with("invalid name")
    );
    assert_eq!(
        err("FROM a b\n").message(),
        "FROM requires either one or three arguments"
    );
    assert_eq!(
        err("FROM\n").message(),
        "FROM requires either one or three arguments"
    );
    assert_eq!(err("FROM --x=1 a\n").message(), "unknown flag: --x");
    // `as` is matched with Go's case folding.
    assert_eq!(
        df("FROM a a\u{17f} b\n").stages[0].name.as_deref(),
        Some("b")
    );
    assert_eq!(
        df("FROM a AS \u{130}\n").stages[0].name.as_deref(),
        Some("i")
    );
}

#[test]
fn instruction_before_from() {
    let e = err("RUN x\n");
    assert_eq!(e.message(), "no build stage in current context");
    assert_eq!(e.lines(), Some(Lines::single(1)));
    assert_eq!(e.kind(), ErrorKind::Instruction);
}

#[test]
fn env_label_arg() {
    let Instruction::Env(kv) = only("ENV A=1 B=\"x y\"").value else {
        panic!()
    };
    assert_eq!(kv.len(), 2);
    assert_eq!(
        err("FROM a\nENV legacy\n").message(),
        "ENV must have two arguments"
    );
    let Instruction::Env(kv) = only("ENV legacy  x y").value else {
        panic!()
    };
    assert_eq!(
        kv,
        [KeyValue {
            key: "legacy".to_owned(),
            value: "x y".to_owned(),
            no_delim: true
        }]
    );
    let Instruction::Label(kv) = only("LABEL a=b c=").value else {
        panic!()
    };
    assert_eq!(kv[1].value, "");
    assert_eq!(
        err("FROM a\nENV =x\n").message(),
        "ENV names can not be blank"
    );
    assert_eq!(
        err("FROM a\nARG =x\n").message(),
        "ARG names can not be blank"
    );
    assert_eq!(
        err("FROM a\nARG\n").message(),
        "ARG requires at least one argument"
    );
    // ARG ignores flags, like STOPSIGNAL.
    assert!(matches!(only("ARG --x=1 a").value, Instruction::Arg(_)));
    assert!(matches!(
        only("STOPSIGNAL --x=1 9").value,
        Instruction::StopSignal(_)
    ));
}

#[test]
fn run_command_and_flags() {
    let Instruction::Run(r) = only("RUN --network=none --mount=type=cache,target=/c echo hi").value
    else {
        panic!()
    };
    assert_eq!(r.cmd.args, ["echo hi"]);
    assert!(r.cmd.prepend_shell);
    assert_eq!(r.network, Network::None);
    assert_eq!(r.mount_specs, ["type=cache,target=/c"]);
    // Only `from` is read before expansion.
    assert_eq!(r.mounts[0].kind, MountType::Bind);
    assert_eq!(r.flags_used, ["mount", "network"]);

    let Instruction::Run(r) = only("RUN [\"a\", \"b\"]").value else {
        panic!()
    };
    assert_eq!(r.cmd.args, ["a", "b"]);
    assert!(!r.cmd.prepend_shell);

    assert_eq!(
        err("FROM a\nRUN --network=bridge x\n").message(),
        "invalid network mode \"bridge\""
    );
    assert_eq!(
        err("FROM a\nRUN --security=x y\n").message(),
        "security \"x\" is not valid"
    );
    assert!(
        err("FROM a\nRUN --mount=from=$X,target=/a x\n")
            .message()
            .contains("doesn't support variable expansion")
    );
}

#[test]
fn run_heredoc_files() {
    let Instruction::Run(r) = only("RUN <<-EOF bash\n\techo hi\nEOF").value else {
        panic!()
    };
    assert_eq!(r.cmd.args, ["<<-EOF bash"]);
    assert_eq!(
        r.cmd.files,
        [InlineFile {
            name: "EOF".to_owned(),
            data: "\techo hi\n".to_owned(),
            chomp: true
        }]
    );
}

#[test]
fn copy_and_add() {
    let Instruction::Copy(c) = only(
        "COPY --from=b --chown=1:2 --chmod=644 --link --parents --exclude=x --exclude=y a <<-'F' /d/\n\tdata $X\nF",
    )
    .value
    else {
        panic!()
    };
    assert_eq!(c.sources, ["a"]);
    assert_eq!(c.dest, "/d/");
    assert_eq!(
        c.contents,
        [SourceContent {
            path: "F".to_owned(),
            data: "data $X\n".to_owned(),
            expand: false
        }]
    );
    assert_eq!(
        (c.from.as_str(), c.chown.as_str(), c.chmod.as_str()),
        ("b", "1:2", "644")
    );
    assert!(c.link && c.parents);
    assert_eq!(c.excludes, ["x", "y"]);

    let Instruction::Add(a) =
        only("ADD --checksum=sha256:x --unpack=false --keep-git-dir u /d").value
    else {
        panic!()
    };
    assert_eq!(a.checksum, "sha256:x");
    assert_eq!((a.unpack, a.keep_git_dir), (Some(false), Some(true)));
    let Instruction::Add(a) = only("ADD u /d").value else {
        panic!()
    };
    assert_eq!((a.unpack, a.keep_git_dir), (None, None));

    assert!(
        err("FROM a\nCOPY a\n")
            .message()
            .starts_with("COPY requires at least two")
    );
    assert_eq!(
        err("FROM a\nCOPY a <<EOF\nx\nEOF\n").message(),
        "COPY cannot accept a heredoc as a destination"
    );
    assert_eq!(
        err("FROM a\nCOPY --foo a b\n").message(),
        "unknown flag: --foo"
    );
    assert_eq!(
        err("FROM a\nADD --from=x a b\n").message(),
        "unknown flag: --from"
    );
}

#[test]
fn cmd_entrypoint_shell() {
    let Instruction::Cmd(c) = only("CMD echo $HOME").value else {
        panic!()
    };
    assert_eq!(c.args, ["echo $HOME"]);
    let Instruction::Entrypoint(e) = only("ENTRYPOINT []").value else {
        panic!()
    };
    assert!(e.args.is_empty() && !e.prepend_shell);
    let Instruction::Shell(s) = only("SHELL [\"bash\", \"-c\"]").value else {
        panic!()
    };
    assert_eq!(s, ["bash", "-c"]);
    assert_eq!(
        err("FROM a\nSHELL bash -c\n").message(),
        "SHELL requires the arguments to be in JSON form"
    );
    assert_eq!(
        err("FROM a\nSHELL []\n").message(),
        "SHELL requires at least one argument"
    );
}

#[test]
fn healthcheck() {
    let Instruction::Healthcheck(h) =
        only("HEALTHCHECK --interval=5s --timeout=1m --retries=3 CMD curl -f x").value
    else {
        panic!()
    };
    assert_eq!(h.test, ["CMD-SHELL", "curl -f x"]);
    assert_eq!(h.interval, Duration::from_secs(5));
    assert_eq!(h.timeout, Duration::from_secs(60));
    assert_eq!(h.retries, 3);
    let Instruction::Healthcheck(h) = only("HEALTHCHECK cmd [\"a\"]").value else {
        panic!()
    };
    assert_eq!(h.test, ["CMD", "a"]);
    let Instruction::Healthcheck(h) = only("HEALTHCHECK --bogus NONE").value else {
        panic!()
    };
    assert_eq!(h.test, ["NONE"]);
    for (src, msg) in [
        ("HEALTHCHECK", "HEALTHCHECK requires at least one argument"),
        ("HEALTHCHECK NONE x", "HEALTHCHECK NONE takes no arguments"),
        (
            "HEALTHCHECK RUN x",
            "Unknown type \"RUN\" in HEALTHCHECK (try CMD)",
        ),
        (
            "HEALTHCHECK CMD []",
            "Missing command after HEALTHCHECK CMD",
        ),
        (
            "HEALTHCHECK --interval=1us CMD x",
            "Interval \"interval\" cannot be less than 1ms",
        ),
        (
            "HEALTHCHECK --interval=-1s CMD x",
            "Interval \"interval\" cannot be less than 1ms",
        ),
        (
            "HEALTHCHECK --retries=-1 CMD x",
            "--retries cannot be negative (-1)",
        ),
        ("HEALTHCHECK --bogus=1 RUN x", "unknown flag: --bogus"),
    ] {
        assert_eq!(err(&format!("FROM a\n{src}\n")).message(), msg, "{src}");
    }
}

#[test]
fn simple_instructions() {
    assert_eq!(
        only("WORKDIR /a b").value,
        Instruction::Workdir("/a b".to_owned())
    );
    assert_eq!(only("USER 1:2").value, Instruction::User("1:2".to_owned()));
    assert_eq!(
        only("EXPOSE 8080 443 80/udp").value,
        Instruction::Expose(vec![
            "443".to_owned(),
            "80/udp".to_owned(),
            "8080".to_owned()
        ])
    );
    assert_eq!(
        only("VOLUME [\" /a \", \"/b\"]").value,
        Instruction::Volume(vec!["/a".to_owned(), "/b".to_owned()])
    );
    assert_eq!(
        only("MAINTAINER me").value,
        Instruction::Maintainer("me".to_owned())
    );
    assert_eq!(
        err("FROM a\nWORKDIR\n").message(),
        "WORKDIR requires exactly one argument"
    );
    assert_eq!(err("FROM a\nUSER --x=1 u\n").message(), "unknown flag: --x");
    assert_eq!(
        err("FROM a\nVOLUME [\"\"]\n").message(),
        "VOLUME specified can not be an empty string"
    );
    assert_eq!(
        err("FROM a\nBOGUS x\n").message(),
        "unknown instruction: BOGUS"
    );
}

#[test]
fn onbuild() {
    let o = only("ONBUILD RUN echo hi");
    assert_eq!(o.value, Instruction::Onbuild("RUN echo hi".to_owned()));
    let o = only("onbuild   copy <<EOF /x\nhello\nEOF");
    assert_eq!(
        o.value,
        Instruction::Onbuild("copy <<EOF /x\nhello\nEOF".to_owned())
    );
    for (src, msg) in [
        ("ONBUILD", "ONBUILD requires at least one argument"),
        (
            "ONBUILD ONBUILD RUN x",
            "Chaining ONBUILD via `ONBUILD ONBUILD` isn't allowed",
        ),
        ("ONBUILD from x", "FROM isn't allowed as an ONBUILD trigger"),
        (
            "ONBUILD MAINTAINER x",
            "MAINTAINER isn't allowed as an ONBUILD trigger",
        ),
    ] {
        assert_eq!(err(&format!("FROM a\n{src}\n")).message(), msg, "{src}");
    }
}

#[test]
fn located_keeps_keyword_code_and_comments() {
    let d = df("FROM a\n# c1\nrun  echo \\\n  x  \n");
    let i = &d.stages[0].instructions[0];
    assert_eq!(i.keyword, "run");
    assert_eq!(i.code, "run  echo   x");
    assert_eq!(i.comments, ["c1"]);
    assert_eq!(i.lines, Lines::new(3, 4));
}

#[test]
fn expand_fields() {
    let lex = Lexer::new('\\');
    let e = env(&[("A", "1"), ("B", "x y"), ("P", "80 443")]);
    let mut i = only("ENV K$A=\"$B\" L=${C:-c}");
    i.expand(&lex, &e).unwrap();
    let Instruction::Env(kv) = &i.value else {
        panic!()
    };
    assert_eq!((kv[0].key.as_str(), kv[0].value.as_str()), ("K1", "x y"));
    assert_eq!(kv[1].value, "c");

    let mut i = only("COPY --chown=$A <<EOF <<'R' $B /d$A\n$B \"q\"\nEOF\n$B\nR");
    i.expand(&lex, &e).unwrap();
    let Instruction::Copy(c) = &i.value else {
        panic!()
    };
    assert_eq!(c.chown, "1");
    assert_eq!(c.sources, ["x y"]);
    assert_eq!(c.dest, "/d1");
    assert_eq!(c.contents[0].data, "x y \"q\"\n");
    assert_eq!(c.contents[1].data, "$B\n");

    let mut i = only("EXPOSE $P 22");
    i.expand(&lex, &e).unwrap();
    assert_eq!(
        i.value,
        Instruction::Expose(vec!["80".to_owned(), "443".to_owned(), "22".to_owned()])
    );

    let mut i = only("RUN --mount=type=$T,target=/$A echo $A");
    let e2 = env(&[("A", "1"), ("T", "cache")]);
    i.expand(&lex, &e2).unwrap();
    let Instruction::Run(r) = &i.value else {
        panic!()
    };
    assert_eq!(r.mounts[0].kind, MountType::Cache);
    assert_eq!(r.mounts[0].target, "/1");
    assert_eq!(r.cmd.args, ["echo $A"]);

    // Errors carry the lines and say what failed.
    let mut i = only("WORKDIR ${X:?unset}");
    let e = i.expand(&lex, &NoEnv).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Expansion);
    assert_eq!(e.lines(), Some(Lines::single(2)));
    let mut i = only("RUN --mount=type=$T x");
    let e = i.expand(&lex, &NoEnv).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Instruction);
}

#[test]
fn expand_uses_the_escape_character() {
    let lex = Lexer::new('`');
    let mut i = only("WORKDIR C:\\a`$X");
    i.expand(&lex, &env(&[("X", "1")])).unwrap();
    assert_eq!(i.value, Instruction::Workdir("C:\\a$X".to_owned()));
}
