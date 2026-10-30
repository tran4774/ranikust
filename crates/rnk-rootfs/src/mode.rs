// Derived from github.com/tonistiigi/dchapes-mode, mode.go and bits.go, at
// v0.0.0-20250318174251-73d941a28323 (the version BuildKit cd8ee244
// vendors), itself a Go translation of BSD's lib/libc/gen/setmode.c.
// Copyright (c) 2016-2018, Dave Chapeskie. All rights reserved.
// Copyright (c) 1989, 1993, 1994 The Regents of the University of California.
// All rights reserved. This code is derived from software contributed to
// Berkeley by Dave Borman at Cray Research, Inc.
// Licensed under the BSD license reproduced in NOTICE ("Code derived from
// dchapes-mode"). Modified for ranikust: translated to Rust, `compress`
// left out (it only merges commands), no umask.

//! `COPY --chmod` modes: an octal number, or a symbolic mode as chmod(1)
//! takes it (`u+x,go-w`, `a=rX`), which BuildKit accepts
//! (`buildkit@cd8ee244 frontend/dockerfile/dockerfile2llb/convert_copy.go:68-88`,
//! Q-120, `ADR-0032`).

use crate::error::{Error, Result};

const IS_UID: u32 = 0o4000;
const IS_GID: u32 = 0o2000;
const IS_TXT: u32 = 0o1000;
const RWXU: u32 = 0o700;
const RWXG: u32 = 0o070;
const RWXO: u32 = 0o007;
const R_ALL: u32 = 0o444;
const W_ALL: u32 = 0o222;
const X_ALL: u32 = 0o111;
const STANDARD: u32 = IS_UID | IS_GID | RWXU | RWXG | RWXO;

/// A `--chmod` value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Chmod {
    /// An octal mode, at most `0o7777`: the permission, setuid, setgid and
    /// sticky bits of every copied entry.
    Octal(u32),
    /// A symbolic mode, applied to each entry's own mode.
    Symbolic(ModeSet),
}

impl Chmod {
    /// Parses `--chmod` as BuildKit does: octal when it reads as an octal
    /// number, else a symbolic mode.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidMode`] for an octal number above `0o7777`, a value
    /// that starts with a digit but is not octal, or a malformed symbolic
    /// mode.
    pub fn parse(s: &str) -> Result<Self> {
        let invalid = |reason| Error::InvalidMode {
            mode: s.to_owned(),
            reason,
        };
        if s.bytes().next().is_some_and(|b| b.is_ascii_digit()) {
            return match u32::from_str_radix(s, 8) {
                Ok(m) if m <= 0o7777 => Ok(Self::Octal(m)),
                _ => Err(invalid("an octal mode must be between 0 and 07777")),
            };
        }
        ModeSet::parse(s)
            .map(Self::Symbolic)
            .ok_or_else(|| invalid("not an octal or symbolic mode"))
    }

    /// The mode an entry whose current permission bits (`& 0o7777`) are
    /// `current` gets; `dir` matters for `X`.
    pub fn apply(&self, current: u32, dir: bool) -> u32 {
        match self {
            Self::Octal(m) => *m & 0o7777,
            Self::Symbolic(set) => set.apply(current & 0o7777, dir),
        }
    }

    /// The mode of the directories a copy creates: an octal mode itself,
    /// else `0o755`, as BuildKit (`fsutil copy.go:85-89`).
    pub fn created_dir_mode(chmod: Option<&Self>) -> u32 {
        match chmod {
            Some(Self::Octal(m)) => *m & 0o7777,
            Some(Self::Symbolic(_)) | None => 0o755,
        }
    }
}

/// A parsed symbolic mode: commands applied in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeSet {
    cmds: Vec<Cmd>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cmd {
    op: u8,
    /// For `u`, `g`, `o`: which classes to set and clear.
    copy: Copy,
    bits: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Copy {
    clear: bool,
    set: bool,
    u: bool,
    g: bool,
    o: bool,
}

impl ModeSet {
    /// setmode(3)'s grammar: `[ugoa]*[+-=][rwxXst]*|[ugo]`, repeated and
    /// separated by `,`; several operations may follow one `who`.
    fn parse(s: &str) -> Option<Self> {
        let s = s.as_bytes();
        let mut set = Self { cmds: Vec::new() };
        if s.is_empty() {
            return None;
        }
        let mut i = 0;
        let mut equal_done = false;
        loop {
            let mut who = 0;
            loop {
                match s.get(i)? {
                    b'a' => who |= STANDARD,
                    b'u' => who |= IS_UID | RWXU,
                    b'g' => who |= IS_GID | RWXG,
                    b'o' => who |= RWXO,
                    _ => break,
                }
                i += 1;
            }
            loop {
                let op = *s.get(i)?;
                i += 1;
                match op {
                    b'+' | b'-' => {}
                    b'=' => equal_done = false,
                    _ => return None,
                }
                who &= !IS_TXT;
                let (mut perm, mut perm_x) = (0, 0);
                loop {
                    let b = s.get(i).copied().unwrap_or(0);
                    match b {
                        b'r' => perm |= R_ALL,
                        b'w' => perm |= W_ALL,
                        b'x' => perm |= X_ALL,
                        b's' if who == 0 || who & !RWXO != 0 => perm |= IS_UID | IS_GID,
                        b't' if who == 0 || who & !RWXO != 0 => {
                            who |= IS_TXT;
                            perm |= IS_TXT;
                        }
                        b's' | b't' => {}
                        b'X' if op == b'-' => perm |= X_ALL,
                        b'X' => perm_x = X_ALL,
                        b'u' | b'g' | b'o' => {
                            if perm != 0 {
                                set.add(op, who, perm);
                                perm = 0;
                            }
                            if op == b'=' {
                                equal_done = true;
                            }
                            if perm_x != 0 {
                                set.add(b'X', who, perm_x);
                                perm_x = 0;
                            }
                            set.add_copy(b, who, op);
                        }
                        _ => {
                            if perm != 0 || (op == b'=' && !equal_done) {
                                if op == b'=' {
                                    equal_done = true;
                                }
                                set.add(op, who, perm);
                            }
                            if perm_x != 0 {
                                set.add(b'X', who, perm_x);
                            }
                            break;
                        }
                    }
                    i += 1;
                }
                match s.get(i) {
                    None => return Some(set),
                    Some(b',') => {
                        i += 1;
                        break;
                    }
                    Some(_) => {}
                }
            }
        }
    }

    /// `addcmd` for `+`, `-`, `=` and `X`.
    fn add(&mut self, op: u8, who: u32, perm: u32) {
        let mask = |bits| if who != 0 { who & bits } else { bits };
        let mut op = op;
        if op == b'=' {
            self.cmds.push(Cmd {
                op: b'-',
                copy: Copy::default(),
                bits: if who != 0 { who } else { STANDARD },
            });
            op = b'+';
        }
        self.cmds.push(Cmd {
            op,
            copy: Copy::default(),
            bits: mask(perm),
        });
    }

    /// `addcmd` for `u`, `g` and `o`: copy one class's bits to `who`.
    fn add_copy(&mut self, from: u8, who: u32, op: u8) {
        let mut copy = Copy {
            clear: op != b'+',
            set: op != b'-',
            ..Copy::default()
        };
        let bits = if who != 0 {
            copy.u = who & 0o400 != 0;
            copy.g = who & 0o040 != 0;
            copy.o = who & 0o004 != 0;
            u32::MAX
        } else {
            copy.u = true;
            copy.g = true;
            copy.o = true;
            u32::MAX
        };
        self.cmds.push(Cmd {
            op: from,
            copy,
            bits,
        });
    }

    /// `Set.Apply`: `X` looks at the mode before any command and at `dir`.
    fn apply(&self, old: u32, dir: bool) -> u32 {
        let mut new = old;
        for c in &self.cmds {
            match c.op {
                b'u' => {
                    let v = (new & RWXU) >> 6;
                    copy_bits(&mut new, c, v);
                }
                b'g' => {
                    let v = (new & RWXG) >> 3;
                    copy_bits(&mut new, c, v);
                }
                b'o' => {
                    let v = new & RWXO;
                    copy_bits(&mut new, c, v);
                }
                b'+' => new |= c.bits,
                b'-' => new &= !c.bits,
                b'X' if old & X_ALL != 0 || dir => new |= c.bits,
                _ => {}
            }
        }
        new & 0o7777
    }
}

/// The `common` closure of `Set.Apply`: clear, then set, `value` (three
/// bits) in the classes the command names.
fn copy_bits(new: &mut u32, c: &Cmd, value: u32) {
    if c.copy.clear {
        let clear = if c.copy.set { RWXO } else { value };
        for (on, shift) in [(c.copy.u, 6), (c.copy.g, 3), (c.copy.o, 0)] {
            if on {
                *new &= !((clear << shift) & c.bits);
            }
        }
    }
    if c.copy.set {
        for (on, shift) in [(c.copy.u, 6), (c.copy.g, 3), (c.copy.o, 0)] {
            if on {
                *new |= (value << shift) & c.bits;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(mode: &str, old: u32, dir: bool) -> u32 {
        Chmod::parse(mode).unwrap().apply(old, dir)
    }

    #[test]
    fn octal_modes() {
        assert_eq!(Chmod::parse("755").unwrap(), Chmod::Octal(0o755));
        assert_eq!(Chmod::parse("04755").unwrap(), Chmod::Octal(0o4755));
        for bad in ["10000", "0789", "9"] {
            assert!(Chmod::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn symbolic_modes_follow_setmode() {
        // Cases checked against chmod(1) on Linux, which shares setmode's
        // grammar; `X` adds execute only to directories and already
        // executable files.
        let cases = [
            ("u+x", 0o644, false, 0o744),
            ("go-w", 0o666, false, 0o644),
            ("a=r", 0o777, false, 0o444),
            ("=r", 0o777, false, 0o444),
            ("u=rwx,go=rx", 0o000, false, 0o755),
            ("a+X", 0o644, false, 0o644),
            ("a+X", 0o644, true, 0o755),
            ("a+X", 0o744, false, 0o755),
            ("o=u", 0o750, false, 0o757),
            ("g+u", 0o700, false, 0o770),
            ("u+s", 0o755, false, 0o4755),
            ("+t", 0o755, true, 0o1755),
            ("o+t", 0o755, true, 0o755),
            ("u+x-w", 0o644, false, 0o544),
            ("a-rwx,u+rw", 0o777, false, 0o600),
        ];
        for (mode, old, dir, want) in cases {
            assert_eq!(apply(mode, old, dir), want, "{mode} on {old:o}");
        }
    }

    #[test]
    fn malformed_symbolic_modes_are_refused() {
        for bad in ["", "u", "u+z", "x+r", "u+r,", "a=r,,u+x", "rwx"] {
            assert!(Chmod::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn created_directories_take_only_an_octal_mode() {
        assert_eq!(Chmod::created_dir_mode(None), 0o755);
        assert_eq!(Chmod::created_dir_mode(Some(&Chmod::Octal(0o700))), 0o700);
        let sym = Chmod::parse("u+x").unwrap();
        assert_eq!(Chmod::created_dir_mode(Some(&sym)), 0o755);
    }
}
