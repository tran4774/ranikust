use super::*;

fn table(rows: &[(i32, i32)]) -> BTreeMap<i32, i32> {
    rows.iter().copied().collect()
}

#[test]
fn the_stat_parent_survives_odd_command_names() {
    assert_eq!(parent_of(b"42 (sh) S 7 42 42 0 -1"), Some(7));
    assert_eq!(parent_of(b"42 (a) b) S 9 1 1"), Some(9));
    assert_eq!(parent_of(b"42 (x y) Z 1 0"), Some(1));
    assert_eq!(parent_of(b"garbage"), None);
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
