//! Forwards `tracing` events to an OpenTelemetry logger.
//!
//! `tracing-opentelemetry` only records an event as a span event, so an event outside any span is
//! dropped, and an event inside a span is exported only when that span closes (never, for a span
//! wrapping a long-lived loop). Exporting every event as a log record removes both limits.
//!
//! This is a trimmed `opentelemetry-appender-tracing` 0.27 bridge plus the span correlation that
//! crate only gained in 0.28, which needs OpenTelemetry 0.28.

//!
//! Both layers decide what they handle inside their own callbacks instead of through
//! `Layer::with_filter`: per-layer filtering adds span-lookup bookkeeping to every span enter, exit
//! and close, measured at roughly +0.6 µs per request on a span-per-request service.

use std::any::TypeId;

use opentelemetry::{
    logs::{AnyValue, LogRecord, Logger, Severity},
    trace::{Status, TraceContextExt},
    Key,
};
use tracing::{field::Field, span, subscriber::Interest, Event, Level, Metadata, Subscriber};
use tracing_opentelemetry::OtelData;
use tracing_subscriber::{filter::LevelFilter, layer::Context, registry::LookupSpan, Layer};

/// Wraps the span layer so it never records events, which the log bridge exports instead.
pub(crate) struct SpansOnly<L>(pub(crate) L);

impl<S: Subscriber, L: Layer<S>> Layer<S> for SpansOnly<L> {
    fn on_register_dispatch(&self, subscriber: &tracing::Dispatch) {
        self.0.on_register_dispatch(subscriber)
    }

    fn on_layer(&mut self, subscriber: &mut S) {
        self.0.on_layer(subscriber)
    }

    fn register_callsite(&self, metadata: &'static Metadata<'static>) -> Interest {
        self.0.register_callsite(metadata)
    }

    fn enabled(&self, metadata: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        self.0.enabled(metadata, ctx)
    }

    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, S>) {
        self.0.on_new_span(attrs, id, ctx)
    }

    fn max_level_hint(&self) -> Option<LevelFilter> {
        self.0.max_level_hint()
    }

    fn on_record(&self, span: &span::Id, values: &span::Record<'_>, ctx: Context<'_, S>) {
        self.0.on_record(span, values, ctx)
    }

    fn on_follows_from(&self, span: &span::Id, follows: &span::Id, ctx: Context<'_, S>) {
        self.0.on_follows_from(span, follows, ctx)
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.0.on_enter(id, ctx)
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, S>) {
        self.0.on_exit(id, ctx)
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, S>) {
        self.0.on_close(id, ctx)
    }

    fn on_id_change(&self, old: &span::Id, new: &span::Id, ctx: Context<'_, S>) {
        self.0.on_id_change(old, new, ctx)
    }

    // `on_event` is deliberately not forwarded.

    // SAFETY: returns a pointer to `self` for its own type id, and otherwise defers to the inner
    // layer's implementation, which upholds the contract for the types it recognizes.
    unsafe fn downcast_raw(&self, id: TypeId) -> Option<*const ()> {
        if id == TypeId::of::<Self>() {
            Some(self as *const Self as *const ())
        } else {
            self.0.downcast_raw(id)
        }
    }
}

pub(crate) struct LogBridge<L> {
    logger: L,
}

impl<L> LogBridge<L> {
    pub(crate) fn new(logger: L) -> Self {
        Self { logger }
    }
}

impl<S, L> Layer<S> for LogBridge<L>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    L: Logger + Send + Sync + 'static,
{
    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();

        // OpenTelemetry's own diagnostics: an export failure would otherwise log, export and fail again in a loop.
        if meta.target().starts_with("opentelemetry") {
            return;
        }

        let mut record = self.logger.create_log_record();
        record.set_target(meta.target());
        record.set_event_name(meta.name());
        record.set_severity_number(severity_of(meta.level()));
        record.set_severity_text(meta.level().as_str());
        event.record(&mut FieldVisitor { record: &mut record });

        if let Some(span) = ctx.event_span(event) {
            // Read first and write after: holding the write lock while reading would deadlock.
            let trace_context = span.extensions().get::<OtelData>().and_then(|otel| {
                let span_id = otel.builder.span_id?;
                let trace_id = if otel.parent_cx.has_active_span() {
                    otel.parent_cx.span().span_context().trace_id()
                } else {
                    otel.builder.trace_id?
                };
                Some((trace_id, span_id))
            });

            if let Some((trace_id, span_id)) = trace_context {
                record.set_trace_context(trace_id, span_id, None);
            }

            // The span layer no longer sees events, so keep its rule that an error event fails the span.
            if *meta.level() == Level::ERROR {
                if let Some(otel) = span.extensions_mut().get_mut::<OtelData>() {
                    if otel.builder.status == Status::Unset {
                        otel.builder.status = Status::error("");
                    }
                }
            }
        }

        self.logger.emit(record);
    }
}

struct FieldVisitor<'a, R> {
    record: &'a mut R,
}

impl<R: LogRecord> tracing::field::Visit for FieldVisitor<'_, R> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let value = format!("{value:?}");
        if field.name() == "message" {
            self.record.set_body(value.into());
        } else {
            self.record.add_attribute(Key::new(field.name()), AnyValue::from(value));
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.record.set_body(value.to_owned().into());
        } else {
            self.record.add_attribute(Key::new(field.name()), AnyValue::from(value.to_owned()));
        }
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record.add_attribute(Key::new(field.name()), AnyValue::from(value));
    }

    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record.add_attribute(Key::new(field.name()), AnyValue::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record.add_attribute(Key::new(field.name()), AnyValue::from(value));
    }
}

const fn severity_of(level: &Level) -> Severity {
    match *level {
        Level::TRACE => Severity::Trace,
        Level::DEBUG => Severity::Debug,
        Level::INFO => Severity::Info,
        Level::WARN => Severity::Warn,
        Level::ERROR => Severity::Error,
    }
}
