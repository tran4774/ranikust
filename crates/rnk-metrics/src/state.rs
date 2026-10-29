//! The aggregates: per span name, per record (stage, instruction, snapshot)
//! and per registry host.
//!
//! Every method takes the time it applies to, so tests can drive the state
//! with synthetic instants. Nothing here reads a clock or a file.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Span that opens a stage record: the stage's whole life in the scheduler.
pub(crate) const STAGE_SPAN: &str = "rnk_sched.stage";
/// Span that opens an instruction record (kaniko's "command").
pub(crate) const INSTRUCTION_SPAN: &str = "rnk_builder.instruction";
/// Span that opens a snapshot record of kind `command`: one instruction's diff.
pub(crate) const DIFF_SPAN: &str = "rnk_snapshot.diff";
/// Span that opens a snapshot record of kind `init`: the baseline walk a
/// kaniko-mode stage starts from.
pub(crate) const BASELINE_SPAN: &str = "rnk_snapshot.baseline";
/// Span of a `RUN` process; its fields feed the kaniko-style `child_*` values.
pub(crate) const RUN_SPAN: &str = "rnk_sandbox.run";

/// Converts a duration to nanoseconds, saturating at `u64::MAX` (584 years).
pub(crate) fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// Whether a numeric field keeps its maximum instead of its sum.
pub(crate) fn is_max_field(name: &str) -> bool {
    name.starts_with("max_")
}

/// Numeric fields by name: the values of one span, or the totals of many.
///
/// A handful of fields per span, so a vector beats a map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Fields(Vec<(&'static str, u64)>);

impl Fields {
    /// Sets a field, replacing an earlier value: `span.record` overwrites.
    pub(crate) fn set(&mut self, name: &'static str, value: u64) {
        match self.0.iter_mut().find(|(n, _)| *n == name) {
            Some(entry) => entry.1 = value,
            None => self.0.push((name, value)),
        }
    }

    /// Adds another span's fields: summed, or the maximum for `max_*`.
    pub(crate) fn merge(&mut self, other: &Fields) {
        for &(name, value) in &other.0 {
            match self.0.iter_mut().find(|(n, _)| *n == name) {
                Some(entry) if is_max_field(name) => entry.1 = entry.1.max(value),
                Some(entry) => entry.1 = entry.1.saturating_add(value),
                None => self.0.push((name, value)),
            }
        }
    }

    /// The value of a field, if any span recorded it.
    pub(crate) fn get(&self, name: &str) -> Option<u64> {
        self.0.iter().find(|(n, _)| *n == name).map(|&(_, v)| v)
    }

    /// The fields in the order they were first recorded.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&'static str, u64)> + '_ {
        self.0.iter().copied()
    }
}

/// Totals of the spans of one name, globally or inside one record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Agg {
    /// Closed spans.
    pub(crate) count: u64,
    /// Sum of their durations; exceeds wall time when they overlap.
    pub(crate) total_ns: u64,
    /// The longest one.
    pub(crate) max_ns: u64,
    /// Their numeric fields, summed (or maximum for `max_*`).
    pub(crate) fields: Fields,
}

impl Agg {
    fn add(&mut self, elapsed_ns: u64, fields: &Fields) {
        self.count += 1;
        self.total_ns = self.total_ns.saturating_add(elapsed_ns);
        self.max_ns = self.max_ns.max(elapsed_ns);
        self.fields.merge(fields);
    }
}

/// A global aggregate, which also knows how long at least one span of its
/// name was open: the union of their intervals, not their sum.
#[derive(Debug, Default)]
pub(crate) struct Phase {
    pub(crate) agg: Agg,
    /// Spans of this name open right now.
    pub(crate) open: u64,
    open_since: Option<Instant>,
    wall_ns: u64,
}

impl Phase {
    fn opened(&mut self, at: Instant) {
        if self.open == 0 {
            self.open_since = Some(at);
        }
        self.open += 1;
    }

    fn closed(&mut self, at: Instant, elapsed_ns: u64, fields: &Fields) {
        self.agg.add(elapsed_ns, fields);
        self.open = self.open.saturating_sub(1);
        if self.open == 0
            && let Some(since) = self.open_since.take()
        {
            self.wall_ns = self
                .wall_ns
                .saturating_add(nanos(at.saturating_duration_since(since)));
        }
    }

    /// Wall time up to `at`, counting spans that are still open.
    pub(crate) fn wall_ns_at(&self, at: Instant) -> u64 {
        let running = self
            .open_since
            .map_or(0, |since| nanos(at.saturating_duration_since(since)));
        self.wall_ns.saturating_add(running)
    }
}

/// What a record stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RecordKind {
    /// A stage, from [`STAGE_SPAN`].
    Stage,
    /// An instruction, from [`INSTRUCTION_SPAN`].
    Command,
    /// A snapshot; the string is kaniko's snapshot kind (`init`, `command`).
    Snapshot(&'static str),
}

impl RecordKind {
    /// The record kind a span name opens, if any.
    pub(crate) fn of_span(name: &str) -> Option<Self> {
        match name {
            STAGE_SPAN => Some(Self::Stage),
            INSTRUCTION_SPAN => Some(Self::Command),
            DIFF_SPAN => Some(Self::Snapshot("command")),
            BASELINE_SPAN => Some(Self::Snapshot("init")),
            _ => None,
        }
    }
}

/// Identity of a record, taken from its span's fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    /// Field `stage`: the stage's index in the Dockerfile.
    pub(crate) stage: Option<i64>,
    /// Field `index`: the instruction's index in its stage.
    pub(crate) index: Option<i64>,
    /// Field `line`: the instruction's line in the Dockerfile.
    pub(crate) line: Option<u64>,
    /// Field `kind`: the instruction keyword (`RUN`, `COPY`).
    pub(crate) kind: Option<String>,
    /// Field `name`: the stage name.
    pub(crate) name: Option<String>,
    /// Field `text`: the instruction as written.
    pub(crate) text: Option<String>,
    /// Field `base`: the stage's base image as written.
    pub(crate) base: Option<String>,
    /// Field `host`: the registry host a span talked to.
    pub(crate) host: Option<String>,
    /// Field `cache_hit`: the instruction's layer came from the cache.
    pub(crate) cache_hit: Option<bool>,
}

/// Everything a span carries: its numeric fields and its identity fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SpanFields {
    pub(crate) numbers: Fields,
    pub(crate) id: Identity,
}

/// The records a span is nested in, nearest of each kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Links {
    pub(crate) stage: Option<usize>,
    pub(crate) command: Option<usize>,
    pub(crate) snapshot: Option<usize>,
    /// An ancestor already counts its time and fields against a host.
    pub(crate) in_host: bool,
}

impl Links {
    /// The links a child of a span with these links, own record and fields
    /// inherits.
    pub(crate) fn for_child(
        self,
        record: Option<(usize, RecordKind)>,
        fields: &SpanFields,
    ) -> Self {
        let mut links = self;
        match record {
            Some((idx, RecordKind::Stage)) => links.stage = Some(idx),
            Some((idx, RecordKind::Command)) => links.command = Some(idx),
            Some((idx, RecordKind::Snapshot(_))) => links.snapshot = Some(idx),
            None => {}
        }
        links.in_host |= fields.id.host.is_some();
        links
    }

    fn records(self) -> impl Iterator<Item = usize> {
        [self.stage, self.command, self.snapshot]
            .into_iter()
            .flatten()
    }
}

/// One stage, instruction or snapshot.
#[derive(Clone, Debug)]
pub(crate) struct Record {
    pub(crate) kind: RecordKind,
    /// Stage index: the span's own, else inherited; -1 when unknown.
    pub(crate) stage: i64,
    /// Instruction index; -1 for stage-level records.
    pub(crate) index: i64,
    pub(crate) id: Identity,
    pub(crate) started: Instant,
    /// `None` while the span is open.
    pub(crate) wall_ns: Option<u64>,
    /// The record span's own numeric fields.
    pub(crate) values: Fields,
    /// Totals of the spans nested in it, by name.
    pub(crate) phases: HashMap<&'static str, Agg>,
}

/// Everything collected so far.
#[derive(Debug, Default)]
pub(crate) struct State {
    pub(crate) phases: HashMap<&'static str, Phase>,
    pub(crate) records: Vec<Record>,
    pub(crate) hosts: HashMap<String, Agg>,
}

impl State {
    /// A span opened. Returns the record it opens, if its name opens one.
    pub(crate) fn open(
        &mut self,
        name: &'static str,
        at: Instant,
        fields: &SpanFields,
        links: Links,
    ) -> Option<(usize, RecordKind)> {
        self.phases.entry(name).or_default().opened(at);
        let kind = RecordKind::of_span(name)?;
        let stage_of = |idx: Option<usize>| idx.and_then(|i| self.records.get(i)).map(|r| r.stage);
        let (stage, index) = match kind {
            RecordKind::Stage => (fields.id.stage.unwrap_or(-1), -1),
            RecordKind::Command => (
                fields
                    .id
                    .stage
                    .or_else(|| stage_of(links.stage))
                    .unwrap_or(-1),
                fields.id.index.unwrap_or(-1),
            ),
            RecordKind::Snapshot(_) => match links.command.and_then(|i| self.records.get(i)) {
                Some(command) => (command.stage, command.index),
                None => (stage_of(links.stage).unwrap_or(-1), -1),
            },
        };
        self.records.push(Record {
            kind,
            stage,
            index,
            id: fields.id.clone(),
            started: at,
            wall_ns: None,
            values: Fields::default(),
            phases: HashMap::new(),
        });
        Some((self.records.len() - 1, kind))
    }

    /// A span closed after `elapsed_ns`, with its final fields.
    pub(crate) fn close(
        &mut self,
        name: &'static str,
        at: Instant,
        elapsed_ns: u64,
        fields: &SpanFields,
        links: Links,
        record: Option<usize>,
    ) {
        self.phases
            .entry(name)
            .or_default()
            .closed(at, elapsed_ns, &fields.numbers);
        for idx in links.records() {
            if let Some(enclosing) = self.records.get_mut(idx) {
                enclosing
                    .phases
                    .entry(name)
                    .or_default()
                    .add(elapsed_ns, &fields.numbers);
            }
        }
        if !links.in_host
            && let Some(host) = &fields.id.host
        {
            self.hosts
                .entry(host.clone())
                .or_default()
                .add(elapsed_ns, &fields.numbers);
        }
        if let Some(own) = record.and_then(|idx| self.records.get_mut(idx)) {
            own.wall_ns = Some(elapsed_ns);
            own.values = fields.numbers.clone();
            // Fields recorded after the span opened (`cache_hit` once the
            // lookup is done) complete the identity.
            own.id = fields.id.clone();
            if own.kind == RecordKind::Stage
                && let Some(stage) = fields.id.stage
            {
                own.stage = stage;
            }
            if own.kind == RecordKind::Command {
                if let Some(stage) = fields.id.stage {
                    own.stage = stage;
                }
                if let Some(index) = fields.id.index {
                    own.index = index;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(pairs: &[(&'static str, u64)]) -> SpanFields {
        let mut fields = SpanFields::default();
        for &(name, value) in pairs {
            fields.numbers.set(name, value);
        }
        fields
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn fields_sum_except_max_fields() {
        let mut total = Fields::default();
        total.merge(&numbers(&[("files", 3), ("max_threads", 4)]).numbers);
        total.merge(&numbers(&[("files", 5), ("max_threads", 2)]).numbers);
        assert_eq!(total.get("files"), Some(8));
        assert_eq!(total.get("max_threads"), Some(4));
        assert_eq!(total.get("bytes_in"), None);
    }

    #[test]
    fn set_overwrites_like_span_record() {
        let mut fields = Fields::default();
        fields.set("files", 1);
        fields.set("files", 7);
        assert_eq!(fields.iter().collect::<Vec<_>>(), [("files", 7)]);
    }

    #[test]
    fn phase_counts_total_max_and_wall_of_overlapping_spans() {
        let t0 = Instant::now();
        let mut state = State::default();
        let none = SpanFields::default();
        // Two spans overlap for 5 ms: [0, 10] and [5, 20], then one at [30, 32].
        state.open("x.op", t0, &none, Links::default());
        state.open("x.op", t0 + ms(5), &none, Links::default());
        state.close(
            "x.op",
            t0 + ms(10),
            10_000_000,
            &none,
            Links::default(),
            None,
        );
        state.close(
            "x.op",
            t0 + ms(20),
            15_000_000,
            &none,
            Links::default(),
            None,
        );
        state.open("x.op", t0 + ms(30), &none, Links::default());
        state.close(
            "x.op",
            t0 + ms(32),
            2_000_000,
            &none,
            Links::default(),
            None,
        );

        let phase = &state.phases["x.op"];
        assert_eq!(phase.agg.count, 3);
        assert_eq!(phase.agg.total_ns, 27_000_000);
        assert_eq!(phase.agg.max_ns, 15_000_000);
        assert_eq!(phase.wall_ns_at(t0 + ms(40)), 22_000_000);
        assert_eq!(phase.open, 0);
    }

    #[test]
    fn open_span_counts_towards_wall_until_report() {
        let t0 = Instant::now();
        let mut state = State::default();
        state.open("x.op", t0, &SpanFields::default(), Links::default());
        let phase = &state.phases["x.op"];
        assert_eq!(phase.open, 1);
        assert_eq!(phase.agg.count, 0);
        assert_eq!(phase.wall_ns_at(t0 + ms(3)), 3_000_000);
    }

    #[test]
    fn nested_spans_charge_every_enclosing_record() {
        let t0 = Instant::now();
        let mut state = State::default();
        let mut stage_fields = SpanFields::default();
        stage_fields.id.stage = Some(2);
        let (stage, kind) = state
            .open(STAGE_SPAN, t0, &stage_fields, Links::default())
            .expect("stage record");
        let in_stage = Links::default().for_child(Some((stage, kind)), &stage_fields);

        let mut instr_fields = SpanFields::default();
        instr_fields.id.index = Some(4);
        let (command, kind) = state
            .open(INSTRUCTION_SPAN, t0, &instr_fields, in_stage)
            .expect("command record");
        let in_command = in_stage.for_child(Some((command, kind)), &instr_fields);

        let (snapshot, kind) = state
            .open(DIFF_SPAN, t0, &SpanFields::default(), in_command)
            .expect("snapshot record");
        let in_snapshot = in_command.for_child(Some((snapshot, kind)), &SpanFields::default());

        let walk = numbers(&[("entries", 100)]);
        state.open("rnk_snapshot.walk", t0, &walk, in_snapshot);
        state.close(
            "rnk_snapshot.walk",
            t0 + ms(1),
            1_000_000,
            &walk,
            in_snapshot,
            None,
        );
        let diff = numbers(&[("files_added", 3)]);
        state.close(
            DIFF_SPAN,
            t0 + ms(2),
            2_000_000,
            &diff,
            in_command,
            Some(snapshot),
        );

        // The snapshot inherits stage and index from the instruction.
        let snap = &state.records[snapshot];
        assert_eq!((snap.stage, snap.index), (2, 4));
        assert_eq!(snap.values.get("files_added"), Some(3));
        assert_eq!(
            snap.phases["rnk_snapshot.walk"].fields.get("entries"),
            Some(100)
        );
        // The walk is charged to the snapshot, the instruction and the stage;
        // the diff to the instruction and the stage, not to itself.
        for idx in [command, stage] {
            let phases = &state.records[idx].phases;
            assert_eq!(phases["rnk_snapshot.walk"].count, 1);
            assert_eq!(phases[DIFF_SPAN].fields.get("files_added"), Some(3));
        }
        assert!(!snap.phases.contains_key(DIFF_SPAN));
        assert_eq!(state.records[command].wall_ns, None);
    }

    #[test]
    fn host_is_counted_once_on_the_outermost_span() {
        let t0 = Instant::now();
        let mut state = State::default();
        let mut blob = numbers(&[("bytes_in", 10)]);
        blob.id.host = Some("registry:5000".to_owned());
        state.open("rnk_registry.pull_blob", t0, &blob, Links::default());
        let inner_links = Links::default().for_child(None, &blob);
        let mut retry = numbers(&[("bytes_in", 10)]);
        retry.id.host = Some("registry:5000".to_owned());
        state.open("rnk_registry.request", t0, &retry, inner_links);
        state.close("rnk_registry.request", t0, 5, &retry, inner_links, None);
        state.close(
            "rnk_registry.pull_blob",
            t0,
            9,
            &blob,
            Links::default(),
            None,
        );

        let host = &state.hosts["registry:5000"];
        assert_eq!(host.count, 1);
        assert_eq!(host.fields.get("bytes_in"), Some(10));
    }

    #[test]
    fn identity_recorded_late_reaches_the_record() {
        let t0 = Instant::now();
        let mut state = State::default();
        let mut fields = SpanFields::default();
        fields.id.stage = Some(1);
        let (idx, _) = state
            .open(INSTRUCTION_SPAN, t0, &fields, Links::default())
            .expect("command record");
        fields.id.cache_hit = Some(true);
        fields.id.index = Some(3);
        state.close(
            INSTRUCTION_SPAN,
            t0 + ms(1),
            1_000_000,
            &fields,
            Links::default(),
            Some(idx),
        );
        let record = &state.records[idx];
        assert_eq!(record.id.cache_hit, Some(true));
        assert_eq!((record.stage, record.index), (1, 3));
        assert_eq!(record.wall_ns, Some(1_000_000));
    }
}
