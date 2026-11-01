use super::*;

fn table(rows: &[(i32, i32)]) -> BTreeMap<i32, Stat> {
    rows.iter()
        .map(|&(pid, ppid)| (pid, Stat { ppid, start: 0 }))
        .collect()
}

/// A stat line with `ppid` and `start` where the kernel puts them.
fn line(name: &str, ppid: i32, start: u64) -> Vec<u8> {
    let mut fields = vec!["S".to_owned(), ppid.to_string()];
    // Fields 5 to 21, then 22 the start time, then more.
    fields.extend((5..=21).map(|_| "0".to_owned()));
    fields.push(start.to_string());
    fields.push("1234".to_owned());
    format!("42 ({name}) {}", fields.join(" ")).into_bytes()
}

#[test]
fn the_stat_line_survives_odd_command_names() {
    let want = Some(Stat { ppid: 7, start: 99 });
    assert_eq!(parse_stat(&line("sh", 7, 99)), want);
    assert_eq!(parse_stat(&line("a) b", 7, 99)), want);
    assert_eq!(parse_stat(&line("x y", 7, 99)), want);
    assert_eq!(parse_stat(b"garbage"), None);
    assert_eq!(parse_stat(b"42 (sh) S 7"), None, "no start time");
}

#[test]
fn this_process_reads_back_unchanged() {
    let me = rustix::process::getpid().as_raw_nonzero().get();
    let first = read_stat(me).unwrap();
    assert_eq!(read_stat(me), Some(first));
    assert!(first.start > 0);
}

#[test]
fn the_run_tree_is_what_hangs_below_new_children() {
    // me = 10; helper 11 (a child from before the RUN) with its own child
    // 12; leader 20, a zombie; 21 a `sleep &` reparented to me; 22 a
    // `setsid` daemon below 21; 30 a process of someone else.
    let t = table(&[
        (1, 0),
        (10, 1),
        (11, 10),
        (12, 11),
        (20, 10),
        (21, 10),
        (22, 21),
        (30, 1),
    ]);
    let tree = run_tree(&t, 10, 20, &[11]);
    assert_eq!(tree.into_iter().collect::<Vec<_>>(), [21, 22]);
}

#[test]
fn a_looping_table_ends() {
    // A table read while processes were dying can hold a cycle.
    let t = table(&[(10, 1), (5, 6), (6, 5)]);
    assert!(run_tree(&t, 10, 0, &[]).is_empty());
}

#[test]
fn a_process_whose_stat_changed_is_not_killed() {
    let mut child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    let stat = read_stat(pid).unwrap();
    let me = rustix::process::getpid().as_raw_nonzero().get();
    // Another start time: a pid reused by another process.
    kill(
        pid,
        Stat {
            start: stat.start + 1,
            ..stat
        },
        me,
    )
    .unwrap();
    assert_eq!(child.try_wait().unwrap(), None, "left alive");
    kill(pid, stat, me).unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success());
}

#[test]
fn ranikusts_own_children_are_listed() {
    let child = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = i32::try_from(child.id()).unwrap();
    let mut child = child;
    let listed = children_of_self().unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(listed.contains(&pid), "{listed:?}");
}

#[test]
fn without_pidfds_only_ranikusts_own_children_get_a_plain_kill() {
    use super::{NoPidfd, without_pidfd};
    use rustix::io::Errno;
    assert_eq!(without_pidfd(Errno::SRCH, 7, 7), NoPidfd::Gone);
    // The pid is another process's thread now (a fork loop recycling
    // pids): the listed process is gone, and nothing is killed.
    assert_eq!(without_pidfd(Errno::INVAL, 7, 7), NoPidfd::Gone);
    assert_eq!(without_pidfd(Errno::NOSYS, 7, 7), NoPidfd::Kill);
    assert_eq!(without_pidfd(Errno::PERM, 7, 7), NoPidfd::Kill);
    assert_eq!(without_pidfd(Errno::NOSYS, 8, 7), NoPidfd::Later);
    assert_eq!(without_pidfd(Errno::PERM, 8, 7), NoPidfd::Later);
    assert_eq!(without_pidfd(Errno::MFILE, 7, 7), NoPidfd::Fail);
}
