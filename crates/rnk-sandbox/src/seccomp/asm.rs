//! A classic BPF assembler for seccomp filters. Jumps go to labels, so no
//! offset is counted by hand, and a conditional jump too far for its 8-bit
//! offsets is laid out through two `ja`, whose offset has 32 bits.

pub(super) const LD_W_ABS: u16 = 0x20;
pub(super) const ALU_AND_K: u16 = 0x54;
pub(super) const JA: u16 = 0x05;
pub(super) const JEQ_K: u16 = 0x15;
pub(super) const JGT_K: u16 = 0x25;
pub(super) const JGE_K: u16 = 0x35;
pub(super) const JSET_K: u16 = 0x45;
pub(super) const RET_K: u16 = 0x06;

/// `BPF_MAXINSNS`: the kernel refuses a longer filter.
pub(super) const MAX_INSNS: usize = 4096;

/// A jump target, bound to the next instruction by [`Asm::bind`].
#[derive(Clone, Copy)]
pub(super) struct Label(usize);

enum Op {
    Ld(u32),
    And(u32),
    Jump(u16, u32, Label, Label),
    Ret(u32),
}

/// Why [`Asm::finish`] made no program.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum AsmError {
    /// More instructions than [`MAX_INSNS`].
    TooLong(usize),
    /// A label unbound, bound twice, or not after its jump: a bug here.
    Label,
}

#[derive(Default)]
pub(super) struct Asm {
    ops: Vec<Op>,
    /// The op each label is bound to.
    labels: Vec<Option<usize>>,
    rebound: bool,
}

impl Asm {
    pub(super) fn label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    pub(super) fn bind(&mut self, l: Label) {
        match self.labels.get_mut(l.0) {
            Some(slot @ None) => *slot = Some(self.ops.len()),
            Some(Some(_)) | None => self.rebound = true,
        }
    }

    pub(super) fn ld(&mut self, offset: u32) {
        self.ops.push(Op::Ld(offset));
    }

    pub(super) fn and(&mut self, mask: u32) {
        self.ops.push(Op::And(mask));
    }

    pub(super) fn jump(&mut self, code: u16, k: u32, t: Label, f: Label) {
        self.ops.push(Op::Jump(code, k, t, f));
    }

    pub(super) fn ret(&mut self, action: u32) {
        self.ops.push(Op::Ret(action));
    }

    /// Goes to `to` when the test holds, else on.
    pub(super) fn on(&mut self, code: u16, k: u32, to: Label) {
        let next = self.label();
        self.jump(code, k, to, next);
        self.bind(next);
    }

    /// Goes to `to` when the test fails, else on.
    pub(super) fn unless(&mut self, code: u16, k: u32, to: Label) {
        let next = self.label();
        self.jump(code, k, next, to);
        self.bind(next);
    }

    /// `A` in `lo..=hi` goes to `t`, else on (the next instruction).
    pub(super) fn in_range(&mut self, (lo, hi): (u32, u32), t: Label) {
        let check_hi = self.label();
        let next = self.label();
        self.jump(JGE_K, lo, check_hi, next);
        self.bind(check_hi);
        self.jump(JGT_K, hi, next, t);
        self.bind(next);
    }

    /// The program. A conditional jump whose target is more than 255
    /// instructions on becomes `jump 0, 1; ja t; ja f`; that only lengthens
    /// the program, so the layout is redone until no jump grows.
    pub(super) fn finish(self) -> Result<Vec<libc::sock_filter>, AsmError> {
        if self.rebound {
            return Err(AsmError::Label);
        }
        let mut long = vec![false; self.ops.len()];
        let start = loop {
            let mut start = Vec::with_capacity(self.ops.len() + 1);
            let mut at = 0;
            for &l in &long {
                start.push(at);
                at += if l { 3 } else { 1 };
            }
            start.push(at);
            if at > MAX_INSNS {
                return Err(AsmError::TooLong(at));
            }
            let mut grew = false;
            for (i, op) in self.ops.iter().enumerate() {
                if let Op::Jump(_, _, t, f) = *op
                    && !long[i]
                {
                    let fits = |l: Label| {
                        self.target(l, &start, i)
                            .and_then(|to| to.checked_sub(start[i] + 1))
                            .is_some_and(|d| d <= usize::from(u8::MAX))
                    };
                    if !fits(t) || !fits(f) {
                        long[i] = true;
                        grew = true;
                    }
                }
            }
            if !grew {
                break start;
            }
        };
        let mut out = Vec::with_capacity(start[self.ops.len()]);
        let insn = |code, jt, jf, k| libc::sock_filter { code, jt, jf, k };
        for (i, op) in self.ops.iter().enumerate() {
            let here = start[i];
            // The offset of `l` from the instruction after `from`.
            let off = |l: Label, from: usize| {
                self.target(l, &start, i)
                    .and_then(|to| to.checked_sub(from + 1))
                    .ok_or(AsmError::Label)
            };
            match *op {
                Op::Ld(k) => out.push(insn(LD_W_ABS, 0, 0, k)),
                Op::And(k) => out.push(insn(ALU_AND_K, 0, 0, k)),
                Op::Ret(k) => out.push(insn(RET_K, 0, 0, k)),
                Op::Jump(code, k, t, f) if long[i] => {
                    let kt = u32::try_from(off(t, here + 1)?).map_err(|_| AsmError::Label)?;
                    let kf = u32::try_from(off(f, here + 2)?).map_err(|_| AsmError::Label)?;
                    out.push(insn(code, 0, 1, k));
                    out.push(insn(JA, 0, 0, kt));
                    out.push(insn(JA, 0, 0, kf));
                }
                Op::Jump(code, k, t, f) => {
                    let jt = u8::try_from(off(t, here)?).map_err(|_| AsmError::Label)?;
                    let jf = u8::try_from(off(f, here)?).map_err(|_| AsmError::Label)?;
                    out.push(insn(code, jt, jf, k));
                }
            }
        }
        Ok(out)
    }

    /// Where `l` lands once laid out, if bound after op `from` and to an
    /// instruction (not past the end).
    fn target(&self, l: Label, start: &[usize], from: usize) -> Option<usize> {
        let op = self.labels.get(l.0).copied().flatten()?;
        (op > from && op < self.ops.len()).then(|| start[op])
    }
}

/// Runs `prog` on one `seccomp_data`, as the kernel would, and gives the
/// action it returns. Test-only, and panics on anything the assembler does
/// not emit.
#[cfg(test)]
pub(super) fn run(prog: &[libc::sock_filter], data: &[u32; 16]) -> u32 {
    let mut a = 0u32;
    let mut pc = 0usize;
    loop {
        let i = prog[pc];
        pc += 1;
        let k = i.k;
        let cond = |hold: bool| usize::from(if hold { i.jt } else { i.jf });
        match i.code {
            LD_W_ABS => {
                assert_eq!(k % 4, 0, "unaligned load");
                a = data[usize::try_from(k / 4).unwrap()];
            }
            ALU_AND_K => a &= k,
            JA => pc += usize::try_from(k).unwrap(),
            JEQ_K => pc += cond(a == k),
            JGT_K => pc += cond(a > k),
            JGE_K => pc += cond(a >= k),
            JSET_K => pc += cond(a & k != 0),
            RET_K => return k,
            code => panic!("instruction {code:#x} at {}", pc - 1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test `ld 0; jeq k ? t : f` whose targets sit `gap` instructions on.
    fn far(gap: usize) -> Vec<libc::sock_filter> {
        let mut a = Asm::default();
        let [t, f] = [a.label(), a.label()];
        a.ld(0);
        a.jump(JEQ_K, 7, t, f);
        for _ in 0..gap {
            a.ret(1);
        }
        a.bind(t);
        a.ret(2);
        for _ in 0..gap {
            a.ret(1);
        }
        a.bind(f);
        a.ret(3);
        a.finish().unwrap()
    }

    fn data(nr: u32) -> [u32; 16] {
        let mut d = [0; 16];
        d[0] = nr;
        d
    }

    #[test]
    fn near_jumps_stay_one_instruction() {
        let p = far(100);
        assert_eq!(p.len(), 2 + 100 + 1 + 100 + 1);
        assert_eq!(run(&p, &data(7)), 2);
        assert_eq!(run(&p, &data(8)), 3);
    }

    #[test]
    fn far_jumps_go_through_ja() {
        for gap in [127, 128, 254, 255, 256, 300, 1000] {
            let p = far(gap);
            assert_eq!(run(&p, &data(7)), 2, "{gap}");
            assert_eq!(run(&p, &data(8)), 3, "{gap}");
        }
        // The false target alone is past 255.
        let p = far(200);
        assert_eq!((p[1].jt, p[1].jf, p[2].code), (0, 1, JA), "expanded");
    }

    /// A jump that grows pushes an earlier near one past 255: the layout
    /// is redone until nothing grows.
    #[test]
    fn growth_is_iterated() {
        let mut a = Asm::default();
        let [x, y, b] = [a.label(), a.label(), a.label()];
        a.ld(0);
        // 255 on from the first jump, until the second grows.
        a.jump(JEQ_K, 1, x, b);
        a.bind(b);
        a.jump(JEQ_K, 2, y, y);
        for _ in 0..254 {
            a.ret(9);
        }
        a.bind(x);
        a.ret(5);
        for _ in 0..300 {
            a.ret(9);
        }
        a.bind(y);
        a.ret(6);
        let p = a.finish().unwrap();
        assert_eq!(p[2].code, JA, "the first jump grew");
        assert_eq!(run(&p, &data(1)), 5);
        assert_eq!(run(&p, &data(0)), 6);
    }

    #[test]
    fn bad_labels_and_length_are_refused() {
        let mut a = Asm::default();
        let l = a.label();
        a.ld(0);
        a.jump(JEQ_K, 0, l, l);
        assert_eq!(a.finish().unwrap_err(), AsmError::Label, "unbound");

        let mut a = Asm::default();
        let l = a.label();
        a.bind(l);
        a.ld(0);
        a.jump(JEQ_K, 0, l, l);
        a.ret(0);
        assert_eq!(a.finish().unwrap_err(), AsmError::Label, "backward");

        let mut a = Asm::default();
        let l = a.label();
        a.ld(0);
        a.jump(JEQ_K, 0, l, l);
        a.bind(l);
        assert_eq!(a.finish().unwrap_err(), AsmError::Label, "past the end");

        let mut a = Asm::default();
        let l = a.label();
        a.bind(l);
        a.bind(l);
        a.ret(0);
        assert_eq!(a.finish().unwrap_err(), AsmError::Label, "bound twice");

        let mut a = Asm::default();
        for _ in 0..=MAX_INSNS {
            a.ret(0);
        }
        assert_eq!(a.finish().unwrap_err(), AsmError::TooLong(MAX_INSNS + 1));
    }
}
