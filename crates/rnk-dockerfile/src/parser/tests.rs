use super::*;

fn ast(src: &str) -> Ast {
    parse_ast(src).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn err(src: &str) -> Error {
    parse_ast(src).expect_err(src)
}

fn args(n: &Node) -> Vec<&str> {
    n.args.iter().map(String::as_str).collect()
}

#[test]
fn directives_at_the_top() {
    let a = ast("# syntax=docker/dockerfile:1\n# escape=`\n# check=skip=all\nFROM x\n");
    assert_eq!(a.escape, '`');
    assert_eq!(a.syntax(), Some("docker/dockerfile:1"));
    assert_eq!(a.check(), Some("skip=all"));
    assert_eq!(a.directives[1].line, 2);
    assert!(
        a.nodes[0].comments.is_empty(),
        "directives are not comments"
    );
}

#[test]
fn directive_after_a_comment_or_blank_line_is_ignored() {
    assert_eq!(ast("# hello\n# escape=`\nFROM x\n").escape, '\\');
    assert_eq!(ast("\n# escape=`\nFROM x\n").escape, '\\');
    assert_eq!(ast("FROM x\n# escape=`\n").escape, '\\');
    // An unknown key ends the directive block too.
    assert_eq!(ast("# foo=bar\n# escape=`\nFROM x\n").escape, '\\');
}

#[test]
fn directive_errors() {
    let e = err("# escape=x\nFROM x\n");
    assert_eq!(e.kind(), ErrorKind::Syntax);
    assert_eq!(e.lines(), Some(Lines::single(1)));
    assert!(e.message().contains("invalid escape token"));
    let e = err("# escape=`\n# ESCAPE=\\\nFROM x\n");
    assert_eq!(e.message(), "only one escape parser directive can be used");
    assert_eq!(e.lines(), Some(Lines::single(2)));
    // Only white space after `=`: the value is that white space.
    assert!(
        err("# escape=  \nFROM x\n")
            .message()
            .contains("invalid escape token ' '")
    );
}

#[test]
fn directive_regex() {
    assert_eq!(match_directive("escape = `  "), Some(("escape", "`")));
    assert_eq!(
        match_directive("syntax=a b\u{b}"),
        Some(("syntax", "a b\u{b}"))
    );
    assert_eq!(match_directive("escape="), None);
    assert_eq!(match_directive("1x=y"), None);
    assert_eq!(match_directive("e-x=y"), None);
}

#[test]
fn byte_order_mark_and_crlf() {
    let a = ast("\u{feff}# escape=`\r\nFROM x\r\nRUN a `\r\n  b\r\n");
    assert_eq!(a.escape, '`');
    assert_eq!(args(&a.nodes[1]), ["a   b"]);
    // Heredoc bodies keep their line endings; the delimiter line may end
    // in CRLF.
    let a = ast("FROM a\r\nCOPY <<EOF /x\r\nline\r\nEOF\r\nRUN b\r\n");
    assert_eq!(a.nodes[1].heredocs[0].content, "line\r\n");
    assert_eq!(a.nodes[2].lines, Lines::single(5));
}

#[test]
fn continuation_comments_and_empty_lines() {
    let src = "FROM x\nRUN a \\\n# comment\n\n   b \\\n  \\\n c\nRUN d\n";
    let a = ast(src);
    assert_eq!(a.nodes.len(), 3);
    assert_eq!(args(&a.nodes[1]), ["a    b    c"]);
    assert_eq!(a.nodes[1].lines, Lines::new(2, 7));
    assert_eq!(a.nodes[2].lines, Lines::single(8));
    assert_eq!(a.warnings.len(), 1);
    assert_eq!(a.warnings[0].line, 7);
}

#[test]
fn escaped_escape_is_not_a_continuation() {
    let a = ast("FROM x\nENV A=b\\\\\nENV C=d\n");
    assert_eq!(a.nodes.len(), 3);
    assert_eq!(args(&a.nodes[1]), ["A", "b\\\\", "="]);
    // A lone escape on a line continues; one at the end of the file too.
    let a = ast("FROM x\nRUN a \\");
    assert_eq!(args(&a.nodes[1]), ["a"]);
}

#[test]
fn comments_attach_to_the_next_instruction() {
    let a = ast("# one\n#\n# two\n  # three\nFROM x\nRUN y\n");
    assert_eq!(a.nodes[0].comments, ["two", "# three"]);
    assert!(a.nodes[1].comments.is_empty());
}

#[test]
fn json_and_shell_forms() {
    let a =
        ast("FROM x\nCMD [\"a\", \"b c\"]\nCMD ['a']\nRUN [ ]\nENTRYPOINT []\nSHELL [\"sh\",\n");
    assert!(a.nodes[1].json);
    assert_eq!(args(&a.nodes[1]), ["a", "b c"]);
    assert!(!a.nodes[2].json);
    assert_eq!(args(&a.nodes[2]), ["['a']"]);
    assert!(a.nodes[3].json && a.nodes[3].args.is_empty());
    assert!(a.nodes[4].json && a.nodes[4].args.is_empty());
    assert!(!a.nodes[5].json);
    let e = err("FROM x\nCMD [\"a\", 1]\n");
    assert_eq!(e.lines(), Some(Lines::single(2)));
    assert!(e.message().contains("strings only"));
}

#[test]
fn list_forms() {
    let a = ast("FROM x AS y\nCOPY a  b\tc\nVOLUME [\"/a\"]\nEXPOSE 80 443/udp\n");
    assert_eq!(args(&a.nodes[0]), ["x", "AS", "y"]);
    assert_eq!(args(&a.nodes[1]), ["a", "b", "c"]);
    assert_eq!(args(&a.nodes[2]), ["/a"]);
    assert_eq!(args(&a.nodes[3]), ["80", "443/udp"]);
}

#[test]
fn env_and_label_forms() {
    let a = ast("FROM x\nENV A=1 B=\"x y\" C=a\\ b\nENV D  e  f\nLABEL \"k\"=v\nARG a b=c\n");
    assert_eq!(
        args(&a.nodes[1]),
        ["A", "1", "=", "B", "\"x y\"", "=", "C", "a\\ b", "="]
    );
    assert_eq!(args(&a.nodes[2]), ["D", "e  f", ""]);
    assert_eq!(args(&a.nodes[3]), ["\"k\"", "v", "="]);
    assert_eq!(args(&a.nodes[4]), ["a", "b=c"]);
    let e = err("FROM x\nENV A\n");
    assert_eq!(e.message(), "ENV must have two arguments");
    let e = err("FROM x\nLABEL a=b c\n");
    assert!(e.message().contains("can't find = in \"c\""), "{e}");
}

#[test]
fn flags() {
    let a = ast(
        "FROM --platform=$P x\nCOPY --from=b --chown=\"a b\" -- --x y\nRUN --mount=type=cache,target=/c --network=none echo\n",
    );
    assert_eq!(a.nodes[0].flags, ["--platform=$P"]);
    assert_eq!(a.nodes[1].flags, ["--from=b", "--chown=a b"]);
    assert_eq!(args(&a.nodes[1]), ["--x", "y"]);
    assert_eq!(
        a.nodes[2].flags,
        ["--mount=type=cache,target=/c", "--network=none"]
    );
    assert_eq!(args(&a.nodes[2]), ["echo"]);
}

#[test]
fn flags_are_read_as_latin1_bytes() {
    // BuildKit reads flags byte by byte; `é` (C3 A9) becomes `Ã©`.
    let a = ast("FROM x\nCOPY --chown=\u{e9} a b\n");
    assert_eq!(a.nodes[1].flags, ["--chown=\u{c3}\u{a9}"]);
}

#[test]
fn healthcheck_and_onbuild() {
    let a = ast(
        "FROM x\nHEALTHCHECK --interval=5s CMD [\"a\"]\nHEALTHCHECK NONE\nONBUILD RUN echo hi\n",
    );
    assert_eq!(args(&a.nodes[1]), ["CMD", "a"]);
    assert!(a.nodes[1].json);
    assert_eq!(args(&a.nodes[2]), ["NONE"]);
    let ob = &a.nodes[3];
    assert!(ob.args.is_empty());
    let child = ob.child.as_deref().expect("child");
    assert_eq!(child.keyword, "RUN");
    assert_eq!(args(child), ["echo hi"]);
    assert_eq!(ob.flat_args(), ["RUN", "echo hi"]);
    assert_eq!(ob.dump(), "onbuild (run \"echo hi\")");
}

#[test]
fn deeply_nested_onbuild_is_an_error() {
    let src = format!("FROM x\n{}RUN y\n", "ONBUILD ".repeat(50));
    assert!(parse_ast(&src).is_err());
}

#[test]
fn unknown_instruction_keeps_an_empty_argument() {
    let a = ast("FROM x\nFOO bar\n");
    assert_eq!(args(&a.nodes[1]), [""]);
    assert_eq!(a.dump(), "(from \"x\")\n(foo \"\")");
}

#[test]
fn heredocs() {
    let src = "FROM x\nRUN <<EOF bash\necho $a\nEOF\nCOPY <<-'A' <<B /dst/\n\tone\n\tA\ntwo\nB\nRUN cat <<EOF > /out && echo \"<<NO\"\nhi\nEOF\n";
    let a = ast(src);
    let run = &a.nodes[1];
    assert_eq!(args(run), ["<<EOF bash"]);
    assert_eq!(run.heredocs.len(), 1);
    let h = &run.heredocs[0];
    assert_eq!((h.name.as_str(), h.expand, h.chomp), ("EOF", true, false));
    assert_eq!(h.content, "echo $a\n");
    assert_eq!(run.lines, Lines::new(2, 4));

    let copy = &a.nodes[2];
    assert_eq!(args(copy), ["<<-'A'", "<<B", "/dst/"]);
    assert_eq!(copy.heredocs[0].name, "A");
    assert!(!copy.heredocs[0].expand && copy.heredocs[0].chomp);
    assert_eq!(copy.heredocs[0].content, "\tone\n");
    assert_eq!(copy.heredocs[1].content, "two\n");
    assert_eq!(copy.lines, Lines::new(5, 9));

    let mid = &a.nodes[3];
    assert_eq!(mid.heredocs.len(), 1, "<< inside quotes is not a heredoc");
    assert_eq!(mid.heredocs[0].content, "hi\n");
    assert_eq!(chomp_heredoc_content("\ta\n\t\tb\nc\t\n"), "a\nb\nc\t\n");
}

#[test]
fn heredoc_details() {
    let h = parse_heredoc("3<< \"E\"OF").unwrap().unwrap();
    assert_eq!(
        (h.name.as_str(), h.file_descriptor, h.expand),
        ("EOF", 3, false)
    );
    assert!(parse_heredoc("<<").unwrap().is_none());
    assert!(parse_heredoc("<<a<b").unwrap().is_none());
    assert!(parse_heredoc("x<<EOF").unwrap().is_none());
    // Heredocs are not read for JSON forms or other instructions.
    let a = ast("FROM x\nRUN [\"cat\", \"<<EOF\"]\nENV a=<<EOF\n");
    assert!(a.nodes[1].heredocs.is_empty() && a.nodes[2].heredocs.is_empty());
}

#[test]
fn unterminated_heredoc() {
    let e = err("FROM x\nRUN <<EOF\nhello\n");
    assert_eq!(e.message(), "unterminated heredoc");
    assert_eq!(e.lines(), Some(Lines::new(2, 3)));
}

#[test]
fn onbuild_heredocs() {
    let a = ast("FROM x\nONBUILD RUN <<EOF\nhi\nEOF\n");
    assert_eq!(a.nodes[1].heredocs.len(), 1);
}

#[test]
fn empty_files() {
    for src in ["", "\n\n", "# comment\n", "# escape=`\n"] {
        let e = err(src);
        assert_eq!(e.message(), "file with no instructions");
        assert!(e.lines().is_some());
    }
}

#[test]
fn line_length_limit() {
    let ok = format!("FROM {}\n", "a".repeat(MAX_LINE - 5));
    assert!(parse_ast(&ok).is_ok());
    let long = format!("FROM x\nRUN {}\n", "a".repeat(MAX_LINE));
    let e = err(&long);
    assert!(e.message().contains("65535"), "{e}");
    assert_eq!(e.lines(), Some(Lines::single(2)));
}

#[test]
fn parse_words_keeps_quotes_and_escapes() {
    let w = |s| parse_words(s, '\\');
    assert_eq!(w("foo bar"), ["foo", "bar"]);
    assert_eq!(w("foo\\ bar"), ["foo\\ bar"]);
    assert_eq!(w("foo bar 'abc xyz'"), ["foo", "bar", "'abc xyz'"]);
    assert_eq!(w("föo bàr \"âbc xÿz\""), ["föo", "bàr", "\"âbc xÿz\""]);
    assert_eq!(w("a=\"\""), ["a=\"\""]);
    assert_eq!(w("a \\"), ["a"]);
}

#[test]
fn extract_flags_stops_at_double_dash_and_non_flags() {
    assert_eq!(
        extract_builder_flags("--a=1 --b=\"x y\" 'x y'", '\\'),
        (
            "'x y'".to_owned(),
            vec!["--a=1".to_owned(), "--b=x y".to_owned()]
        )
    );
    assert_eq!(
        extract_builder_flags("--a=1 -- --b", '\\'),
        (" --b".to_owned(), vec!["--a=1".to_owned()])
    );
    assert_eq!(
        extract_builder_flags("--a=\"\" x", '\\'),
        ("x".to_owned(), vec!["--a=".to_owned()])
    );
    assert_eq!(extract_builder_flags("-a", '\\'), ("-a".to_owned(), vec![]));
}
