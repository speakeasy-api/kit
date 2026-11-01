//! Request-local W3C context. Metadata is never recorded as span attributes.
use opentelemetry::{
    Context,
    baggage::{BaggageExt, KeyValueMetadata},
    trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState},
};
use serde_json::{Map, Value};
use tracing_opentelemetry::OpenTelemetrySpanExt;

pub(crate) const TARGET: &str = "kit::telemetry::incoming";

pub(crate) fn span(meta: Option<&Map<String, Value>>) -> tracing::Span {
    let span = tracing::info_span!(target: TARGET, parent: None, "acp.request");
    let _ = span.set_parent(context(meta));
    span
}

pub(crate) fn operation(meta: Option<&Map<String, Value>>) -> tracing::Span {
    let span = crate::telemetry::error_spans::operation("acp");
    let _ = span.set_parent(context(meta));
    span
}

fn context(meta: Option<&Map<String, Value>>) -> Context {
    let mut context = Context::new();
    let Some(meta) = meta else { return context };
    if let Some(parent) = meta
        .get("traceparent")
        .and_then(Value::as_str)
        .and_then(traceparent)
    {
        let state = meta
            .get("tracestate")
            .and_then(Value::as_str)
            .and_then(tracestate)
            .unwrap_or_default();
        context = context
            .with_remote_span_context(SpanContext::new(parent.0, parent.1, parent.2, true, state));
    }
    if let Some(value) = meta
        .get("baggage")
        .and_then(Value::as_str)
        .filter(|s| s.len() <= 8192)
    {
        // Baggage stays in context, not exported attributes or model/tool inputs.
        // Parse locally: the SDK propagator logs raw malformed baggage values.
        let entries = value.split(',').take(64).filter_map(baggage_member);
        context = context.with_baggage(entries);
    }
    context
}

fn traceparent(value: &str) -> Option<(TraceId, SpanId, TraceFlags)> {
    let value = value.trim_matches([' ', '\t']);
    if value.len() < 55 || value.len() > 512 {
        return None;
    }
    let bytes = value.as_bytes();
    if bytes[2] != b'-' || bytes[35] != b'-' || bytes[52] != b'-' {
        return None;
    }
    for range in [0..2, 3..35, 36..52, 53..55] {
        if !bytes[range]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        {
            return None;
        }
    }
    let version = u8::from_str_radix(&value[..2], 16).ok()?;
    if version == 255
        || (version == 0 && value.len() != 55)
        || (value.len() > 55
            && (bytes[55] != b'-'
                || value.len() == 56
                || !bytes[56..].iter().all(u8::is_ascii_graphic)))
    {
        return None;
    }
    let trace = TraceId::from_hex(&value[3..35]).ok()?;
    let parent = SpanId::from_hex(&value[36..52]).ok()?;
    if trace == TraceId::INVALID || parent == SpanId::INVALID {
        return None;
    }
    let flags = TraceFlags::new(u8::from_str_radix(&value[53..55], 16).ok()?) & TraceFlags::SAMPLED;
    Some((trace, parent, flags))
}

fn tracestate(value: &str) -> Option<TraceState> {
    if value.len() > 512 {
        return None;
    }
    let mut pairs = Vec::new();
    for member in value.split(',') {
        let (key, value) = member.trim_matches([' ', '\t']).split_once('=')?;
        let valid_segment = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-*/".contains(&b))
        };
        let valid_key = if let Some((tenant, system)) = key.split_once('@') {
            tenant.len() <= 241
                && system.len() <= 14
                && valid_segment(tenant)
                && valid_segment(system)
                && tenant.as_bytes()[0].is_ascii_alphanumeric()
                && system.as_bytes()[0].is_ascii_lowercase()
        } else {
            key.len() <= 256 && valid_segment(key) && key.as_bytes()[0].is_ascii_lowercase()
        };
        if !valid_key
            || value.is_empty()
            || value.len() > 256
            || !value
                .bytes()
                .all(|b| (0x20..=0x7e).contains(&b) && b != b',' && b != b'=')
            || value.ends_with(' ')
            || pairs.iter().any(|(k, _)| *k == key)
            || pairs.len() == 32
        {
            return None;
        }
        pairs.push((key, value));
    }
    TraceState::from_key_value(pairs).ok()
}

fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

fn baggage_member(member: &str) -> Option<KeyValueMetadata> {
    if member.len() > 4096
        || !member
            .bytes()
            .all(|b| b.is_ascii_graphic() || b == 32 || b == 9)
    {
        return None;
    }
    let mut parts = member.trim().split(';');
    let (key, value) = parts.next()?.split_once('=')?;
    let key = key.trim();
    let value = value.trim();
    if !token(key) {
        return None;
    }
    let valid_value = |s: &str| {
        s.bytes()
            .all(|b| b.is_ascii_graphic() && !b"\",;\\".contains(&b))
    };
    if !valid_value(value) {
        return None;
    }
    let mut properties = Vec::new();
    for property in parts {
        let property = property.trim();
        let (name, value) = property
            .split_once('=')
            .map_or((property, None), |(k, v)| (k.trim(), Some(v.trim())));
        if !token(name) || value.is_some_and(|v| !valid_value(v)) {
            return None;
        }
        properties.push(property);
    }
    Some(KeyValueMetadata::new(
        key.to_owned(),
        decode(value)?,
        properties.join(";"),
    ))
}

fn decode(value: &str) -> Option<String> {
    let mut bytes = value.bytes();
    let mut output = Vec::with_capacity(value.len());
    while let Some(byte) = bytes.next() {
        output.push(if byte == b'%' {
            let hi = (bytes.next()? as char).to_digit(16)?;
            let lo = (bytes.next()? as char).to_digit(16)?;
            (hi * 16 + lo) as u8
        } else {
            byte
        });
    }
    String::from_utf8(output).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    const PARENT: &str = "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01";

    #[test]
    fn validates_parent_and_state() {
        assert!(traceparent(PARENT).is_some());
        for bad in [
            PARENT.replace("00-", "ff-"),
            PARENT.to_uppercase(),
            format!("{PARENT}-extra"),
            PARENT.replace(
                "4bf92f3577b34da6a3ce929d0e0e4736",
                "00000000000000000000000000000000",
            ),
            PARENT.replace("00f067aa0ba902b7", "0000000000000000"),
        ] {
            assert!(traceparent(&bad).is_none(), "{bad}");
        }
        assert_eq!(
            tracestate("vendor=value, other=ok").unwrap().header(),
            "vendor=value,other=ok"
        );
        for bad in [
            "vendor=a,vendor=b",
            "=a",
            "vendor=a=b",
            "vendor=secret\n",
            "1vendor=a",
            "a@=b",
        ] {
            assert!(tracestate(bad).is_none(), "{bad}");
        }
        assert!(tracestate(&vec!["vendor=a"; 33].join(",")).is_none());
    }

    #[test]
    fn extracts_only_reserved_context_without_attributes() {
        let meta = serde_json::json!({"traceparent": PARENT, "tracestate": "vendor=ok", "baggage": "region=west%20coast;public,broken,secret=%ff", "authorization": "do-not-export"});
        let context = context(meta.as_object());
        let span = context.span();
        assert!(span.span_context().is_remote());
        assert_eq!(
            span.span_context().trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(span.span_context().trace_state().header(), "vendor=ok");
        assert_eq!(
            context.baggage().get("region").unwrap().as_str(),
            "west coast"
        );
        assert_eq!(context.baggage().len(), 1);
        assert!(!super::context(None).span().span_context().is_valid());
        assert!(super::context(None).baggage().is_empty());
    }

    #[derive(Debug)]
    struct Capture(std::sync::mpsc::Sender<opentelemetry_sdk::trace::SpanData>);

    impl opentelemetry_sdk::trace::SpanProcessor for Capture {
        fn on_start(&self, _: &mut opentelemetry_sdk::trace::Span, _: &Context) {}
        fn on_end(&self, span: opentelemetry_sdk::trace::SpanData) {
            let _ = self.0.send(span);
        }
        fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }
        fn shutdown_with_timeout(
            &self,
            _: std::time::Duration,
        ) -> opentelemetry_sdk::error::OTelSdkResult {
            Ok(())
        }
    }

    pub(crate) fn capture() -> (
        tracing::Dispatch,
        std::sync::mpsc::Receiver<opentelemetry_sdk::trace::SpanData>,
        opentelemetry_sdk::trace::SdkTracerProvider,
    ) {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::{Layer as _, prelude::*};
        let (tx, rx) = std::sync::mpsc::channel();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_span_processor(Capture(tx))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_filter(crate::telemetry::exported_targets()),
        );
        (tracing::Dispatch::new(subscriber), rx, provider)
    }

    #[test]
    fn exported_operations_join_remote_trace_without_exporting_metadata() {
        use opentelemetry::trace::TracerProvider as _;
        use tracing_subscriber::{Layer as _, prelude::*};
        let (tx, rx) = std::sync::mpsc::channel();
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
            .with_span_processor(Capture(tx))
            .build();
        let subscriber = tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("test"))
                .with_filter(crate::telemetry::exported_targets()),
        );
        tracing::subscriber::with_default(subscriber, || {
            let meta = serde_json::json!({"traceparent": PARENT, "tracestate": "vendor=ok", "baggage": "secret=private", "authorization": "private"});
            {
                let operation = operation(meta.as_object());
                assert_eq!(
                    operation
                        .context()
                        .baggage()
                        .get("secret")
                        .unwrap()
                        .as_str(),
                    "private"
                );
                let _entered = operation.enter();
                let child = tracing::info_span!(target: "agentkit_loop", "invoke_agent");
                let _child = child.enter();
            }
            // A subsequent request without metadata must not inherit the prior caller.
            drop(operation(None));
        });
        let spans = rx.try_iter().collect::<Vec<_>>();
        assert_eq!(spans.len(), 3);
        let child = &spans[0];
        let parent = &spans[1];
        assert_eq!(parent.parent_span_id.to_string(), "00f067aa0ba902b7");
        assert_eq!(
            parent.span_context.trace_id().to_string(),
            "4bf92f3577b34da6a3ce929d0e0e4736"
        );
        assert_eq!(child.parent_span_id, parent.span_context.span_id());
        assert_eq!(
            child.span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(parent.span_context.trace_state().header(), "vendor=ok");
        assert_ne!(
            spans[2].span_context.trace_id(),
            parent.span_context.trace_id()
        );
        assert_eq!(spans[2].parent_span_id, SpanId::INVALID);
        for span in spans {
            assert!(!format!("{:?}", span.attributes).contains("private"));
        }
    }

    #[test]
    fn malformed_and_oversized_metadata_is_ignored() {
        for meta in [
            serde_json::json!({"traceparent": 42, "tracestate": "vendor=ok", "baggage": ["secret"]}),
            serde_json::json!({"traceparent": "invalid", "tracestate": "vendor=ok", "baggage": "x".repeat(8193)}),
        ] {
            let cx = context(meta.as_object());
            assert!(!cx.span().span_context().is_valid());
            assert!(cx.span().span_context().trace_state().header().is_empty());
            assert!(cx.baggage().is_empty());
        }
        let meta = serde_json::json!({"traceparent": PARENT, "tracestate": "vendor=a,vendor=b"});
        let cx = context(meta.as_object());
        assert!(cx.span().span_context().is_valid());
        assert!(cx.span().span_context().trace_state().header().is_empty());
        assert!(
            !traceparent(&PARENT.replace("-01", "-00"))
                .unwrap()
                .2
                .is_sampled()
        );
        assert!(traceparent(&PARENT.replacen("00-", "01-", 1)).is_some());
        assert!(traceparent(&format!("{}-extension", PARENT.replacen("00-", "01-", 1))).is_some());
        for value in [
            "region=a\n",
            "region=%",
            "region=%zz",
            "region=%ff",
            "region=x;bad property",
            "region=x;key=\"quoted\"",
        ] {
            assert!(baggage_member(value).is_none(), "{value}");
        }
    }
}
