//! Forwards `tracing` events to an OpenTelemetry logger.
//!
//! `tracing-opentelemetry` only records an event as a span event, so an event outside any span is
//! dropped, and an event inside a span is exported only when that span closes (never, for a span
//! wrapping a long-lived loop). Exporting every event as a log record removes both limits.
//!
//! This is a trimmed `opentelemetry-appender-tracing` 0.27 bridge plus the span correlation that
//! crate only gained in 0.28, which needs OpenTelemetry 0.28.

use opentelemetry::{
    logs::{AnyValue, LogRecord, Logger, Severity},
    trace::{Status, TraceContextExt},
    Key,
};
use tracing::{field::Field, Event, Level, Subscriber};
use tracing_opentelemetry::OtelData;
use tracing_subscriber::{layer::Context, registry::LookupSpan, Layer};

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
