//! Log output of the gateway binary (M6): human-readable text (default) or
//! one JSON object per line for log pipelines (`--log-format json`).
//!
//! JSON lines look like
//! `{"ts":"2026-10-21T09:00:00.000000Z","level":"WARN","target":"wsprism_redis","msg":"...","shard":2,"span":{"trace_id":"...","t":"acme"}}`:
//! the event's fields at the top level (never replacing `ts`, `level`,
//! `target`, `msg` or `span`), the fields of the spans it happened in (outer
//! first, inner wins) under `span`. Built on `tracing-subscriber`'s
//! formatting hooks and `serde_json`, without another dependency.

use std::fmt;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormattedFields};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::EnvFilter;

/// How log lines are written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// One readable line per event (tracing-subscriber's default).
    Text,
    /// One JSON object per line.
    Json,
}

/// Install the global subscriber: `RUST_LOG` filters, `format` writes.
pub fn init(format: LogFormat) {
    let builder = tracing_subscriber::fmt().with_env_filter(EnvFilter::from_default_env());
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder.event_format(Json).fmt_fields(JsonFields).init(),
    }
}

const RESERVED: [&str; 5] = ["ts", "level", "target", "msg", "span"];

/// The JSON event formatter.
pub struct Json;

impl<S, N> FormatEvent<S, N> for Json
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut ts = String::new();
        SystemTime.format_time(&mut Writer::new(&mut ts))?;
        let meta = event.metadata();
        let mut line = Map::new();
        line.insert("ts".into(), ts.into());
        line.insert("level".into(), meta.level().as_str().into());
        line.insert("target".into(), meta.target().into());

        let mut fields = Fields::default();
        event.record(&mut fields);
        line.insert(
            "msg".into(),
            fields.message.take().unwrap_or_default().into(),
        );
        for (k, v) in fields.map {
            if !RESERVED.contains(&k.as_str()) {
                line.insert(k, v);
            }
        }

        let mut spans = Map::new();
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let ext = span.extensions();
                let Some(f) = ext.get::<FormattedFields<N>>() else {
                    continue;
                };
                // Written by `JsonFields`; with another field formatter
                // the text is not JSON and is kept as is.
                match serde_json::from_str::<Map<String, Value>>(f) {
                    Ok(m) => spans.extend(m),
                    Err(_) if !f.is_empty() => {
                        spans.insert(span.name().into(), f.to_string().into());
                    }
                    Err(_) => {}
                }
            }
        }
        if !spans.is_empty() {
            line.insert("span".into(), Value::Object(spans));
        }
        writeln!(writer, "{}", Value::Object(line))
    }
}

/// Span fields as a JSON object (read back by `Json`).
pub struct JsonFields;

impl<'w> FormatFields<'w> for JsonFields {
    fn format_fields<R: RecordFields>(&self, mut writer: Writer<'w>, fields: R) -> fmt::Result {
        let mut f = Fields::default();
        fields.record(&mut f);
        write!(writer, "{}", Value::Object(f.into_map()))
    }

    fn add_fields(
        &self,
        current: &'w mut FormattedFields<Self>,
        fields: &tracing::span::Record<'_>,
    ) -> fmt::Result {
        // `span.record(..)` after creation: merge into the object.
        let mut map =
            serde_json::from_str::<Map<String, Value>>(&current.fields).unwrap_or_default();
        let mut f = Fields::default();
        fields.record(&mut f);
        map.extend(f.into_map());
        current.fields = Value::Object(map).to_string();
        Ok(())
    }
}

#[derive(Default)]
struct Fields {
    message: Option<String>,
    map: Map<String, Value>,
}

impl Fields {
    fn into_map(mut self) -> Map<String, Value> {
        if let Some(m) = self.message.take() {
            self.map.insert("message".into(), m.into());
        }
        self.map
    }
}

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = Some(value.to_string());
        } else {
            self.map.insert(field.name().into(), value.into());
        }
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.map.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.map.insert(field.name().into(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.map.insert(field.name().into(), value.into());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.map.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            self.message = Some(text);
        } else {
            self.map.insert(field.name().into(), text.into());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn lines(f: impl FnOnce()) -> Vec<Value> {
        let buf = Buf::default();
        let out = buf.clone();
        let sub = tracing_subscriber::fmt()
            .event_format(Json)
            .fmt_fields(JsonFields)
            .with_writer(move || out.clone())
            .finish();
        tracing::subscriber::with_default(sub, f);
        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        text.lines()
            .map(|l| serde_json::from_str(l).expect(l))
            .collect()
    }

    #[test]
    fn events_are_json_lines_with_their_span_fields() {
        let out = lines(|| {
            let outer = tracing::info_span!("ws", trace_id = "abc", t = "acme");
            let _o = outer.enter();
            let inner = tracing::info_span!("room", room = "chat:1", t = "inner");
            let _i = inner.enter();
            tracing::warn!(shard = 2, ok = true, level = "spoof", "it \"broke\": {}", 7);
        });
        assert_eq!(out.len(), 1);
        let l = &out[0];
        assert_eq!(l["level"], "WARN");
        assert_eq!(l["msg"], "it \"broke\": 7");
        assert_eq!(l["shard"], 2);
        assert_eq!(l["ok"], true);
        assert!(l["ts"].as_str().unwrap().ends_with('Z'), "{l}");
        assert!(l["target"].as_str().unwrap().contains("logging"));
        assert_eq!(
            l["span"],
            serde_json::json!({"trace_id":"abc","t":"inner","room":"chat:1"})
        );
    }

    #[test]
    fn fields_recorded_later_are_merged() {
        let out = lines(|| {
            let span = tracing::info_span!("ws", u = tracing::field::Empty, s = "x");
            span.record("u", "alice");
            let _g = span.enter();
            tracing::info!("hello");
        });
        assert_eq!(out[0]["span"], serde_json::json!({"s":"x","u":"alice"}));
        assert!(out[0].get("message").is_none());
    }
}
