//! The JSON report: building it from [`State`], canonical bytes, the file
//! write and the human summary.
//!
//! Section names and units follow kaniko's `metrics.json`
//! (`kaniko@88b6d2b75 pkg/metrics/report.go`) where the meaning matches;
//! `docs/design/metrics.md#comparing-with-kaniko` lists the differences.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::layer::truncate;
use crate::state::{Agg, Phase, RUN_SPAN, Record, RecordKind, State, nanos};
use crate::time::rfc3339;

/// Version of the report format; bumped on any incompatible change.
pub const SCHEMA: &str = "ranikust.metrics/v1";

/// Longest `exit.message` kept, as kaniko.
const MAX_MESSAGE_LEN: usize = 500;
/// Longest instruction `text` kept, as kaniko.
const MAX_TEXT_LEN: usize = 120;
/// Spans listed in the human summary.
const SUMMARY_TOP: usize = 8;

/// How the process ends, for the report's `exit` section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    status: Status,
    code: i32,
    message: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Success,
    Error,
    Panic,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Panic => "panic",
        }
    }
}

impl Outcome {
    /// The process succeeds and exits with code 0.
    pub fn success() -> Self {
        Self {
            status: Status::Success,
            code: 0,
            message: None,
        }
    }

    /// The process fails with `code`; `message` is the error as shown to the
    /// user, cut to 500 characters. It must not hold secrets, like every
    /// error ranikust prints.
    pub fn error(code: i32, message: impl fmt::Display) -> Self {
        Self {
            status: Status::Error,
            code,
            message: Some(message.to_string()),
        }
    }

    /// A panic. Release builds abort (`panic = "abort"`), which a shell sees
    /// as 134 (128 + `SIGABRT`); unwinding builds exit with 101.
    pub(crate) fn panic(message: String) -> Self {
        Self {
            status: Status::Panic,
            code: if cfg!(panic = "abort") { 134 } else { 101 },
            message: Some(message),
        }
    }
}

/// Everything the report needs besides the span state.
pub(crate) struct ReportInput<'a> {
    pub(crate) started: Instant,
    pub(crate) ended: Instant,
    pub(crate) started_wall: SystemTime,
    pub(crate) ended_wall: SystemTime,
    /// `meta` entries known before exit: host, process, `set_meta` values.
    pub(crate) meta: Map<String, Value>,
    pub(crate) outcome: &'a Outcome,
    /// Panic hooks that ran before this report (unwinding builds only).
    pub(crate) panics: u64,
    pub(crate) resources: Value,
}

/// Builds the whole report. `state` is `None` when the panic hook could not
/// take the state lock; the span sections are then empty and
/// `exit.spans_unavailable` says why.
pub(crate) fn build(state: Option<&State>, input: &ReportInput<'_>) -> Value {
    let mut meta = input.meta.clone();
    meta.insert("start".to_owned(), Value::from(rfc3339(input.started_wall)));
    meta.insert("end".to_owned(), Value::from(rfc3339(input.ended_wall)));
    meta.insert(
        "wall_ns".to_owned(),
        Value::from(nanos(input.ended.saturating_duration_since(input.started))),
    );

    let mut exit = Map::new();
    exit.insert(
        "status".to_owned(),
        Value::from(input.outcome.status.as_str()),
    );
    exit.insert("code".to_owned(), Value::from(input.outcome.code));
    if let Some(message) = &input.outcome.message {
        exit.insert(
            "message".to_owned(),
            Value::from(shorten(message, MAX_MESSAGE_LEN)),
        );
    }
    if input.panics > 0 {
        exit.insert("panics".to_owned(), Value::from(input.panics));
    }

    let mut report = match state {
        Some(state) => spans(state, input.started, input.ended),
        None => {
            exit.insert("spans_unavailable".to_owned(), Value::Bool(true));
            spans(&State::default(), input.started, input.ended)
        }
    };
    report.insert("schema".to_owned(), Value::from(SCHEMA));
    report.insert("meta".to_owned(), Value::Object(meta));
    report.insert("exit".to_owned(), Value::Object(exit));
    report.insert("resources".to_owned(), input.resources.clone());
    Value::Object(report)
}

/// The span sections: `phases`, `stages`, `commands`, `snapshots` and
/// `network`. Times are relative to `started`; spans still open count up to
/// `ended`.
pub(crate) fn spans(state: &State, started: Instant, ended: Instant) -> Map<String, Value> {
    let phases: Map<String, Value> = state
        .phases
        .iter()
        .map(|(name, phase)| ((*name).to_owned(), global_phase(phase, ended)))
        .collect();

    let mut stages = Vec::new();
    let mut commands = Vec::new();
    let mut snapshots = Vec::new();
    for record in &state.records {
        match record.kind {
            RecordKind::Stage => stages.push(record),
            RecordKind::Command => commands.push(record),
            RecordKind::Snapshot(_) => snapshots.push(record),
        }
    }
    let order = |a: &&Record, b: &&Record| {
        (a.stage, a.index, a.started).cmp(&(b.stage, b.index, b.started))
    };
    stages.sort_by(order);
    commands.sort_by(order);
    snapshots.sort_by(order);
    let records = |list: Vec<&Record>| -> Value {
        Value::Array(
            list.into_iter()
                .map(|r| record(r, started, ended))
                .collect(),
        )
    };

    let hosts: Map<String, Value> = state
        .hosts
        .iter()
        .map(|(host, agg)| (host.clone(), Value::Object(agg_fields(agg))))
        .collect();

    let mut out = Map::new();
    out.insert("phases".to_owned(), Value::Object(phases));
    out.insert("stages".to_owned(), records(stages));
    out.insert("commands".to_owned(), records(commands));
    out.insert("snapshots".to_owned(), records(snapshots));
    out.insert("network".to_owned(), json!({ "hosts": hosts }));
    out
}

/// `count`, `total_ns`, `max_ns` and the summed fields. The fixed keys are
/// inserted last, so a field that reuses one of their names cannot hide them.
fn agg_fields(agg: &Agg) -> Map<String, Value> {
    let mut out: Map<String, Value> = agg
        .fields
        .iter()
        .map(|(name, value)| (name.to_owned(), Value::from(value)))
        .collect();
    out.insert("count".to_owned(), Value::from(agg.count));
    out.insert("total_ns".to_owned(), Value::from(agg.total_ns));
    out.insert("max_ns".to_owned(), Value::from(agg.max_ns));
    out
}

fn global_phase(phase: &Phase, ended: Instant) -> Value {
    let mut out = agg_fields(&phase.agg);
    out.insert("wall_ns".to_owned(), Value::from(phase.wall_ns_at(ended)));
    if phase.open > 0 {
        out.insert("open".to_owned(), Value::from(phase.open));
    }
    Value::Object(out)
}

fn record(r: &Record, started: Instant, ended: Instant) -> Value {
    let wall_ns = r
        .wall_ns
        .unwrap_or_else(|| nanos(ended.saturating_duration_since(r.started)));
    let mut out = Map::new();
    // kaniko numbers bake targets; ranikust builds one target (ADR-0008).
    out.insert("build".to_owned(), Value::from(0));
    out.insert("stage".to_owned(), Value::from(r.stage));
    out.insert("index".to_owned(), Value::from(r.index));
    if let Some(line) = r.id.line {
        out.insert("line".to_owned(), Value::from(line));
    }
    let kind = match r.kind {
        RecordKind::Stage => Some("stage"),
        RecordKind::Command => r.id.kind.as_deref(),
        RecordKind::Snapshot(kind) => Some(kind),
    };
    if let Some(kind) = kind {
        out.insert("kind".to_owned(), Value::from(kind));
    }
    if let Some(name) = &r.id.name {
        out.insert("name".to_owned(), Value::from(name.as_str()));
    }
    // kaniko keeps a stage's base image in `text`.
    let text = match r.kind {
        RecordKind::Stage => r.id.base.as_deref(),
        RecordKind::Command | RecordKind::Snapshot(_) => r.id.text.as_deref(),
    };
    if let Some(text) = text {
        out.insert("text".to_owned(), Value::from(shorten(text, MAX_TEXT_LEN)));
    }
    if r.id.cache_hit == Some(true) {
        out.insert("cache_hit".to_owned(), Value::Bool(true));
    }
    out.insert(
        "start_ns".to_owned(),
        Value::from(nanos(r.started.saturating_duration_since(started))),
    );
    out.insert("wall_ns".to_owned(), Value::from(wall_ns));
    if r.wall_ns.is_none() {
        out.insert("incomplete".to_owned(), Value::Bool(true));
    }

    let mut values: Map<String, Value> = r
        .values
        .iter()
        .filter(|&(name, _)| name != "cache_hit")
        .map(|(name, value)| (name.to_owned(), Value::from(value)))
        .collect();
    if r.kind == RecordKind::Command {
        child_values(r, wall_ns, &mut values);
    }
    if !values.is_empty() {
        out.insert("values".to_owned(), Value::Object(values));
    }
    if !r.phases.is_empty() {
        let phases: Map<String, Value> = r
            .phases
            .iter()
            .map(|(name, agg)| ((*name).to_owned(), Value::Object(agg_fields(agg))))
            .collect();
        out.insert("phases".to_owned(), Value::Object(phases));
    }
    Value::Object(out)
}

/// kaniko's `child_*` values and `overhead_ns` of a command
/// (`kaniko@88b6d2b75 pkg/metrics/metrics.go:515-543`), from the
/// `rnk_sandbox.run` spans nested in the instruction.
fn child_values(r: &Record, wall_ns: u64, values: &mut Map<String, Value>) {
    let Some(run) = r.phases.get(RUN_SPAN) else {
        return;
    };
    let mut put = |key: &str, value: u64| {
        values.entry(key.to_owned()).or_insert(Value::from(value));
    };
    put("child_count", run.count);
    put("child_wall_ns", run.total_ns);
    put("overhead_ns", wall_ns.saturating_sub(run.total_ns));
    for (field, key) in [
        ("user_ns", "child_user_ns"),
        ("sys_ns", "child_sys_ns"),
        ("max_rss_kb", "child_maxrss_kb"),
        ("minflt", "child_minflt"),
        ("majflt", "child_majflt"),
        ("inblock", "child_inblock"),
        ("oublock", "child_oublock"),
        ("nvcsw", "child_nvcsw"),
        ("nivcsw", "child_nivcsw"),
    ] {
        if let Some(value) = run.fields.get(field) {
            put(key, value);
        }
    }
}

/// Collapses whitespace (heredocs become one line) and cuts to `max`
/// characters with `...`, as kaniko does.
fn shorten(text: &str, max: usize) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() <= max {
        return joined;
    }
    let mut cut = truncate(joined, max.saturating_sub(3));
    cut.push_str("...");
    cut
}

/// Rebuilds every object with its keys in sorted order, so the output does
/// not depend on whether some crate enables `serde_json/preserve_order`.
pub(crate) fn canonical(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, canonical(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}

/// The report as written: canonical JSON, sorted keys, two-space indent, a
/// final newline.
pub(crate) fn to_bytes(report: &Value) -> serde_json::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(&canonical(report.clone()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

/// Writes through a temporary file in the same directory and renames it, so
/// a reader never sees a partial report. Creates the parent directory, as
/// kaniko does.
pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the metrics path has no file name",
        )
    })?;
    fs::create_dir_all(dir)
        .map_err(|err| io::Error::new(err.kind(), format!("creating {}: {err}", dir.display())))?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let tmp = dir.join(format!(
        ".{}.{}.{unique}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    // `create_new` (O_EXCL): never truncate or follow a file that is already
    // there, a symlink included.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&tmp)?;
    let written = file.write_all(bytes).and_then(|()| fs::rename(&tmp, path));
    drop(file);
    if written.is_err() {
        // Best effort: the write error below is what the user needs to see.
        if let Err(cleanup) = fs::remove_file(&tmp)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            crate::warn(&format!("removing {}: {cleanup}", tmp.display()));
        }
    }
    written
}

/// A few lines for a person: outcome, CPU, peak memory, stages and the spans
/// that took the most time.
pub(crate) fn summary(report: &Value, file: Option<&Path>) -> String {
    let u = |pointer: &str| report.pointer(pointer).and_then(Value::as_u64);
    let mut out = String::new();
    let status = report
        .pointer("/exit/status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let code = report
        .pointer("/exit/code")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    out.push_str(&format!(
        "ranikust metrics: {status} (exit {code}) in {}",
        secs(u("/meta/wall_ns").unwrap_or(0))
    ));
    if let Some(file) = file {
        out.push_str(&format!("; report: {}", file.display()));
    }
    out.push('\n');

    if let (Some(su), Some(ss), Some(cu), Some(cs)) = (
        u("/resources/rusage_self/user_ns"),
        u("/resources/rusage_self/sys_ns"),
        u("/resources/rusage_children/user_ns"),
        u("/resources/rusage_children/sys_ns"),
    ) {
        out.push_str(&format!(
            "  cpu: self {} user + {} sys; children {} user + {} sys",
            secs(su),
            secs(ss),
            secs(cu),
            secs(cs)
        ));
        out.push('\n');
    }
    if let (Some(own), Some(child)) = (
        u("/resources/rusage_self/maxrss_kb"),
        u("/resources/rusage_children/maxrss_kb"),
    ) {
        out.push_str(&format!(
            "  peak rss: self {} MiB, largest child {} MiB",
            own / 1024,
            child / 1024
        ));
        if let Some(peak) = u("/resources/end/cgroup/values/memory.peak") {
            out.push_str(&format!(
                "; cgroup memory.peak {} MiB",
                peak / (1024 * 1024)
            ));
        }
        out.push('\n');
    }

    if let Some(stages) = report.get("stages").and_then(Value::as_array)
        && !stages.is_empty()
    {
        let list: Vec<String> = stages
            .iter()
            .map(|s| {
                format!(
                    "{} {} {}",
                    s.get("stage").and_then(Value::as_i64).unwrap_or(-1),
                    s.get("name").and_then(Value::as_str).unwrap_or("-"),
                    secs(s.get("wall_ns").and_then(Value::as_u64).unwrap_or(0))
                )
            })
            .collect();
        out.push_str(&format!("  stages: {}", list.join(", ")));
        out.push('\n');
    }

    if let Some(phases) = report.get("phases").and_then(Value::as_object) {
        let total = |v: &Value| v.get("total_ns").and_then(Value::as_u64).unwrap_or(0);
        let mut top: Vec<(&String, &Value)> = phases.iter().collect();
        top.sort_by(|a, b| total(b.1).cmp(&total(a.1)).then(a.0.cmp(b.0)));
        if !top.is_empty() {
            out.push_str("  spans by total time:\n");
        }
        for (name, phase) in top.into_iter().take(SUMMARY_TOP) {
            let count = phase.get("count").and_then(Value::as_u64).unwrap_or(0);
            let wall = phase.get("wall_ns").and_then(Value::as_u64).unwrap_or(0);
            out.push_str(&format!(
                "    {name:<32} {count:>6}x {:>10} total {:>10} wall",
                secs(total(phase)),
                secs(wall)
            ));
            out.push('\n');
        }
    }
    out
}

/// Nanoseconds as seconds with two decimals, by integer arithmetic.
fn secs(ns: u64) -> String {
    format!(
        "{}.{:02} s",
        ns / 1_000_000_000,
        ns % 1_000_000_000 / 10_000_000
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::state::{DIFF_SPAN, INSTRUCTION_SPAN, Links, STAGE_SPAN, SpanFields};

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// One stage with one `RUN` instruction and its diff, built through the
    /// state API with synthetic times.
    fn sample_state(t0: Instant) -> State {
        let mut state = State::default();
        let mut stage = SpanFields::default();
        stage.id.stage = Some(0);
        stage.id.name = Some("deps".to_owned());
        stage.id.base = Some("node:20-alpine".to_owned());
        let (s, sk) = state
            .open(STAGE_SPAN, t0, &stage, Links::default())
            .expect("stage");
        let in_stage = Links::default().for_child(Some((s, sk)), &stage);

        let mut instr = SpanFields::default();
        instr.id.index = Some(3);
        instr.id.line = Some(7);
        instr.id.kind = Some("RUN".to_owned());
        instr.id.text = Some("RUN  npm\n   ci".to_owned());
        let (c, ck) = state
            .open(INSTRUCTION_SPAN, t0 + ms(1), &instr, in_stage)
            .expect("command");
        let in_command = in_stage.for_child(Some((c, ck)), &instr);

        let mut run = SpanFields::default();
        run.numbers.set("user_ns", 7_000);
        run.numbers.set("sys_ns", 1_000);
        run.numbers.set("max_rss_kb", 2_048);
        state.open(RUN_SPAN, t0 + ms(1), &run, in_command);
        state.close(RUN_SPAN, t0 + ms(6), 5_000_000, &run, in_command, None);

        let mut diff = SpanFields::default();
        diff.numbers.set("files_added", 12);
        diff.numbers.set("whiteouts", 1);
        let (d, _) = state
            .open(DIFF_SPAN, t0 + ms(6), &diff, in_command)
            .expect("diff");
        state.close(DIFF_SPAN, t0 + ms(8), 2_000_000, &diff, in_command, Some(d));

        let mut pull = SpanFields::default();
        pull.numbers.set("bytes_in", 1_000);
        pull.numbers.set("requests", 2);
        pull.id.host = Some("registry:5000".to_owned());
        state.open("rnk_registry.pull_blob", t0, &pull, in_stage);
        state.close(
            "rnk_registry.pull_blob",
            t0 + ms(1),
            1_000_000,
            &pull,
            in_stage,
            None,
        );

        instr.id.cache_hit = Some(false);
        state.close(
            INSTRUCTION_SPAN,
            t0 + ms(9),
            8_000_000,
            &instr,
            in_stage,
            Some(c),
        );
        state
    }

    fn input(t0: Instant, outcome: &Outcome) -> ReportInput<'_> {
        let mut meta = Map::new();
        meta.insert("pid".to_owned(), Value::from(42));
        ReportInput {
            started: t0,
            ended: t0 + ms(10),
            started_wall: UNIX_EPOCH + Duration::from_secs(1_793_318_400),
            ended_wall: UNIX_EPOCH + Duration::from_secs(1_793_318_400) + ms(10),
            meta,
            outcome,
            panics: 0,
            resources: json!({ "rusage_self": { "user_ns": 1 } }),
        }
    }

    #[test]
    fn report_matches_kaniko_sections() {
        let t0 = Instant::now();
        let state = sample_state(t0);
        let outcome = Outcome::success();
        let report = build(Some(&state), &input(t0, &outcome));

        assert_eq!(report["schema"], SCHEMA);
        assert_eq!(report["exit"], json!({ "status": "success", "code": 0 }));
        assert_eq!(report["meta"]["wall_ns"], 10_000_000);
        assert_eq!(report["meta"]["start"], "2026-10-30T00:00:00.000000000Z");

        let stage = &report["stages"][0];
        assert_eq!(stage["kind"], "stage");
        assert_eq!(stage["name"], "deps");
        assert_eq!(stage["text"], "node:20-alpine");
        assert_eq!(stage["incomplete"], true);
        assert_eq!(stage["wall_ns"], 10_000_000);

        let command = &report["commands"][0];
        assert_eq!(command["kind"], "RUN");
        assert_eq!(command["text"], "RUN npm ci");
        assert_eq!(
            (command["stage"].as_i64(), command["index"].as_i64()),
            (Some(0), Some(3))
        );
        assert_eq!(command["line"], 7);
        assert!(command.get("cache_hit").is_none());
        assert_eq!(command["values"]["child_count"], 1);
        assert_eq!(command["values"]["child_wall_ns"], 5_000_000);
        assert_eq!(command["values"]["overhead_ns"], 3_000_000);
        assert_eq!(command["values"]["child_maxrss_kb"], 2_048);
        assert_eq!(command["phases"][DIFF_SPAN]["files_added"], 12);

        let snapshot = &report["snapshots"][0];
        assert_eq!(snapshot["kind"], "command");
        assert_eq!(
            snapshot["values"],
            json!({ "files_added": 12, "whiteouts": 1 })
        );
        assert_eq!(
            (snapshot["stage"].as_i64(), snapshot["index"].as_i64()),
            (Some(0), Some(3))
        );

        assert_eq!(report["phases"][RUN_SPAN]["count"], 1);
        assert_eq!(report["phases"][STAGE_SPAN]["open"], 1);
        assert_eq!(
            report["network"]["hosts"]["registry:5000"],
            json!({ "bytes_in": 1_000, "requests": 2, "count": 1, "total_ns": 1_000_000, "max_ns": 1_000_000 })
        );
    }

    #[test]
    fn canonical_bytes_are_stable_and_sorted() {
        let t0 = Instant::now();
        let outcome = Outcome::error(1, "boom");
        let first = to_bytes(&build(Some(&sample_state(t0)), &input(t0, &outcome))).expect("json");
        let second = to_bytes(&build(Some(&sample_state(t0)), &input(t0, &outcome))).expect("json");
        assert_eq!(first, second);

        let mut unsorted = Map::new();
        unsorted.insert("z".to_owned(), json!({ "b": 1, "a": [{ "d": 1, "c": 2 }] }));
        unsorted.insert("a".to_owned(), Value::from("x"));
        let bytes = to_bytes(&Value::Object(unsorted)).expect("json");
        let expected = "{\n  \"a\": \"x\",\n  \"z\": {\n    \"a\": [\n      {\n        \"c\": 2,\n        \"d\": 1\n      }\n    ],\n    \"b\": 1\n  }\n}\n";
        assert_eq!(String::from_utf8(bytes).expect("utf-8"), expected);
    }

    #[test]
    fn exact_report_bytes() {
        let t0 = Instant::now();
        let mut state = State::default();
        let mut fields = SpanFields::default();
        fields.numbers.set("bytes_out", 5);
        state.open("rnk_layer.encode", t0, &fields, Links::default());
        state.close(
            "rnk_layer.encode",
            t0 + ms(2),
            2_000_000,
            &fields,
            Links::default(),
            None,
        );
        let outcome = Outcome::success();
        let mut input = input(t0, &outcome);
        input.resources = json!({});
        let bytes = to_bytes(&build(Some(&state), &input)).expect("json");
        let expected = r#"{
  "commands": [],
  "exit": {
    "code": 0,
    "status": "success"
  },
  "meta": {
    "end": "2026-10-30T00:00:00.010000000Z",
    "pid": 42,
    "start": "2026-10-30T00:00:00.000000000Z",
    "wall_ns": 10000000
  },
  "network": {
    "hosts": {}
  },
  "phases": {
    "rnk_layer.encode": {
      "bytes_out": 5,
      "count": 1,
      "max_ns": 2000000,
      "total_ns": 2000000,
      "wall_ns": 2000000
    }
  },
  "resources": {},
  "schema": "ranikust.metrics/v1",
  "snapshots": [],
  "stages": []
}
"#;
        assert_eq!(String::from_utf8(bytes).expect("utf-8"), expected);
    }

    #[test]
    fn missing_state_is_reported_not_hidden() {
        let t0 = Instant::now();
        let outcome = Outcome::panic("panic at x.rs:1: boom".to_owned());
        let report = build(None, &input(t0, &outcome));
        assert_eq!(report["exit"]["status"], "panic");
        assert_eq!(report["exit"]["spans_unavailable"], true);
        assert_eq!(report["phases"], json!({}));
    }

    #[test]
    fn long_messages_and_texts_are_cut() {
        assert_eq!(shorten(&"x".repeat(130), 120).chars().count(), 120);
        assert!(shorten(&"x".repeat(130), 120).ends_with("..."));
        assert_eq!(shorten("RUN a\n  && b", 120), "RUN a && b");
    }

    #[test]
    fn write_file_replaces_atomically_and_reports_errors() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("out/metrics.json");
        write_file(&path, b"{}\n").expect("first write");
        write_file(&path, b"{\"a\":1}\n").expect("second write");
        assert_eq!(fs::read(&path).expect("read"), b"{\"a\":1}\n");
        let leftovers: Vec<_> = fs::read_dir(path.parent().expect("parent"))
            .expect("read_dir")
            .collect();
        assert_eq!(leftovers.len(), 1);

        let blocker = tmp.path().join("file");
        fs::write(&blocker, b"").expect("write");
        assert!(write_file(&blocker.join("metrics.json"), b"{}").is_err());
    }

    #[test]
    fn summary_lists_outcome_and_top_spans() {
        let t0 = Instant::now();
        let state = sample_state(t0);
        let outcome = Outcome::success();
        let report = build(Some(&state), &input(t0, &outcome));
        let text = summary(&report, Some(Path::new("/tmp/m.json")));
        assert!(
            text.starts_with("ranikust metrics: success (exit 0) in 0.01 s; report: /tmp/m.json\n")
        );
        assert!(text.contains("stages: 0 deps 0.01 s"));
        assert!(text.contains("rnk_builder.instruction"));
        assert_eq!(secs(1_234_567_890), "1.23 s");
    }
}
