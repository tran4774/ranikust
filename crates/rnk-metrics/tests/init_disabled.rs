//! `init` with metrics off, in a process of its own: the global subscriber
//! is the log layer alone. One test only, so no other subscriber in this
//! process can change tracing's callsite cache.

use tracing::level_filters::LevelFilter;
use tracing::{Level, span};

#[test]
fn metrics_off_installs_nothing_and_spans_stay_disabled() {
    let telemetry = rnk_metrics::init(rnk_metrics::Config::default());
    assert!(!telemetry.is_enabled());
    // The log layer's level is the global maximum: `debug_span!` is
    // rejected by one comparison before any callsite is registered.
    assert_eq!(LevelFilter::current(), LevelFilter::INFO);
    assert!(span!(target: "rnk_layer::encode", Level::DEBUG, "rnk_layer.encode").is_disabled());
    assert!(span!(target: "rnk_layer::encode", Level::TRACE, "rnk_layer.encode").is_disabled());
    // Finishing without metrics writes nothing and does not fail.
    telemetry.finish(&rnk_metrics::Outcome::success());
}
