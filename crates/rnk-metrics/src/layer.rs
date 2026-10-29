//! The `tracing_subscriber::Layer` that feeds [`State`].
//!
//! Per span it keeps a few numeric fields and its start time in the span's
//! extensions; at close it takes the state lock once. It never sees events
//! and never times anything finer than a span.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Metadata, Subscriber};
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

use crate::state::{Links, RecordKind, SpanFields, State, nanos};

/// Longest string kept from an identity field; `text` is cut further in the
/// report.
const MAX_STRING_LEN: usize = 1024;

/// The state shared by the layer, the report and the panic hook.
#[derive(Debug, Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
}

impl Shared {
    /// Locks the state, also after a panic poisoned the lock: the counters
    /// are still worth reporting.
    pub(crate) fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Locks the state if no one holds it. The panic hook uses this: the
    /// panicking thread may hold the lock itself.
    pub(crate) fn try_lock(&self) -> Option<MutexGuard<'_, State>> {
        match self.state.try_lock() {
            Ok(guard) => Some(guard),
            Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        }
    }
}

/// Whether the metrics layer wants a callsite: spans of ranikust's own crates.
/// Dependencies (hyper, h2, tokio) open spans per frame or per poll; those
/// stay disabled.
pub(crate) fn is_metric_span(meta: &Metadata<'_>) -> bool {
    let target = meta.target();
    meta.is_span()
        && (target.starts_with("rnk_") || target == "ranikust" || target.starts_with("ranikust::"))
}

/// Aggregates spans into [`Shared`].
#[derive(Debug)]
pub(crate) struct MetricsLayer {
    shared: Arc<Shared>,
}

impl MetricsLayer {
    pub(crate) fn new(shared: Arc<Shared>) -> Self {
        Self { shared }
    }
}

/// What the layer keeps in a span's extensions.
struct SpanState {
    name: &'static str,
    started: Instant,
    fields: SpanFields,
    links: Links,
    record: Option<(usize, RecordKind)>,
}

impl<S> Layer<S> for MetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut fields = SpanFields::default();
        attrs.record(&mut FieldVisitor(&mut fields));
        let links = span
            .parent()
            .and_then(|parent| {
                parent
                    .extensions()
                    .get::<SpanState>()
                    .map(|p| p.links.for_child(p.record, &p.fields))
            })
            .unwrap_or_default();
        let name = attrs.metadata().name();
        let started = Instant::now();
        let record = self.shared.lock().open(name, started, &fields, links);
        span.extensions_mut().insert(SpanState {
            name,
            started,
            fields,
            links,
            record,
        });
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        if let Some(state) = extensions.get_mut::<SpanState>() {
            values.record(&mut FieldVisitor(&mut state.fields));
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(state) = span.extensions_mut().remove::<SpanState>() else {
            return;
        };
        let now = Instant::now();
        let elapsed = nanos(now.saturating_duration_since(state.started));
        self.shared.lock().close(
            state.name,
            now,
            elapsed,
            &state.fields,
            state.links,
            state.record.map(|(idx, _)| idx),
        );
    }
}

/// Sorts a span's fields into numbers and identity.
struct FieldVisitor<'a>(&'a mut SpanFields);

impl FieldVisitor<'_> {
    fn number(&mut self, field: &Field, value: u64) {
        // An index above `i64::MAX` is no index: `.ok()` leaves it unknown.
        match field.name() {
            "stage" => self.0.id.stage = i64::try_from(value).ok(),
            "index" => self.0.id.index = i64::try_from(value).ok(),
            "line" => self.0.id.line = Some(value),
            name => self.0.numbers.set(name, value),
        }
    }

    fn string(&mut self, field: &Field, value: String) {
        let slot = match field.name() {
            "kind" => &mut self.0.id.kind,
            "name" => &mut self.0.id.name,
            "text" => &mut self.0.id.text,
            "base" => &mut self.0.id.base,
            "host" => &mut self.0.id.host,
            _ => return,
        };
        *slot = Some(truncate(value, MAX_STRING_LEN));
    }
}

impl Visit for FieldVisitor<'_> {
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.number(field, value);
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        match field.name() {
            "stage" => self.0.id.stage = Some(value),
            "index" => self.0.id.index = Some(value),
            // Negative counts mean nothing: dropped.
            _ => {
                if let Ok(value) = u64::try_from(value) {
                    self.number(field, value);
                }
            }
        }
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.number(field, u64::try_from(value).unwrap_or(u64::MAX));
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        // Negative counts are dropped, as in `record_i64`; huge ones saturate.
        if value >= 0 {
            self.number(field, u64::try_from(value).unwrap_or(u64::MAX));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        if field.name() == "cache_hit" {
            self.0.id.cache_hit = Some(value);
        }
        // Counted in aggregates: the sum is how many spans had it true.
        self.0.numbers.set(field.name(), u64::from(value));
    }

    fn record_f64(&mut self, _field: &Field, _value: f64) {
        // Integers only: a float sum is order-dependent and the units of the
        // report are integers (ns, bytes, counts).
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.string(field, value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        // `%value` and `?value` arrive here; only identity fields are kept.
        if matches!(field.name(), "kind" | "name" | "text" | "base" | "host") {
            self.string(field, format!("{value:?}"));
        }
    }
}

/// Cuts a string to at most `max` characters.
pub(crate) fn truncate(mut s: String, max: usize) -> String {
    if let Some((cut, _)) = s.char_indices().nth(max) {
        s.truncate(cut);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_whole_characters() {
        assert_eq!(truncate("héllo".to_owned(), 2), "hé");
        assert_eq!(truncate("abc".to_owned(), 5), "abc");
    }
}
