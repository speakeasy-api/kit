//! Optional OpenTelemetry trace export and resolved host settings.

use agentkit_loop::{MessageCapture, TelemetryConfig};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::{Protocol as OtlpProtocol, WithExportConfig as _};
use std::{fmt, str::FromStr, sync::mpsc, thread, time::Duration};
use tracing_subscriber::{
    Layer as _,
    filter::{LevelFilter, Targets},
    prelude::*,
};

pub const DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES: usize = 64;
pub const DEFAULT_MESSAGE_CONTENT_MAX_BYTES: usize = 16_384;
pub const MAX_MESSAGE_CONTENT_MESSAGES: usize = 1_024;
pub const MAX_MESSAGE_CONTENT_BYTES: usize = 1_048_576;

/// Supported OTLP transports for trace export.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
pub enum Protocol {
    #[default]
    #[serde(rename = "grpc")]
    Grpc,
    #[serde(rename = "http/protobuf")]
    HttpProtobuf,
    #[serde(rename = "http/json")]
    HttpJson,
}

impl fmt::Display for Protocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Grpc => "grpc",
            Self::HttpProtobuf => "http/protobuf",
            Self::HttpJson => "http/json",
        })
    }
}

impl FromStr for Protocol {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "grpc" => Ok(Self::Grpc),
            "http/protobuf" => Ok(Self::HttpProtobuf),
            "http/json" => Ok(Self::HttpJson),
            _ => Err(format!(
                "invalid OTLP trace protocol {value:?}; expected grpc, http/protobuf, or http/json"
            )),
        }
    }
}

/// Fully resolved telemetry settings. Kit owns environment, TOML, and CLI
/// resolution; AgentKit receives no process environment configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub endpoint: Option<String>,
    pub protocol: Protocol,
    pub capture_message_content: bool,
    pub message_content_max_messages: usize,
    pub message_content_max_bytes: usize,
}

impl Settings {
    pub fn try_new(
        endpoint: Option<String>,
        capture_message_content: bool,
        message_content_max_messages: usize,
        message_content_max_bytes: usize,
    ) -> Result<Self, String> {
        Self::try_new_with_protocol(
            endpoint,
            Protocol::default(),
            capture_message_content,
            message_content_max_messages,
            message_content_max_bytes,
        )
    }

    pub fn try_new_with_protocol(
        endpoint: Option<String>,
        protocol: Protocol,
        capture_message_content: bool,
        message_content_max_messages: usize,
        message_content_max_bytes: usize,
    ) -> Result<Self, String> {
        if !(1..=MAX_MESSAGE_CONTENT_MESSAGES).contains(&message_content_max_messages) {
            return Err(format!(
                "otel_message_content_max_messages must be between 1 and {MAX_MESSAGE_CONTENT_MESSAGES}"
            ));
        }
        if !(1..=MAX_MESSAGE_CONTENT_BYTES).contains(&message_content_max_bytes) {
            return Err(format!(
                "otel_message_content_max_bytes must be between 1 and {MAX_MESSAGE_CONTENT_BYTES}"
            ));
        }
        let endpoint = endpoint.filter(|value| !value.is_empty());
        let endpoint = match (endpoint, protocol) {
            (Some(endpoint), Protocol::HttpProtobuf | Protocol::HttpJson) => {
                Some(http_trace_endpoint(&endpoint)?)
            }
            (endpoint, Protocol::Grpc) | (endpoint @ None, _) => endpoint,
        };
        Ok(Self {
            endpoint,
            protocol,
            capture_message_content,
            message_content_max_messages,
            message_content_max_bytes,
        })
    }

    /// Converts the resolved host settings into AgentKit's explicit inference
    /// telemetry configuration. Message capture remains off unless enabled.
    pub fn agentkit_config(&self) -> Result<TelemetryConfig, String> {
        if !(1..=MAX_MESSAGE_CONTENT_MESSAGES).contains(&self.message_content_max_messages) {
            return Err(format!(
                "otel_message_content_max_messages must be between 1 and {MAX_MESSAGE_CONTENT_MESSAGES}"
            ));
        }
        if !(1..=MAX_MESSAGE_CONTENT_BYTES).contains(&self.message_content_max_bytes) {
            return Err(format!(
                "otel_message_content_max_bytes must be between 1 and {MAX_MESSAGE_CONTENT_BYTES}"
            ));
        }
        if !self.capture_message_content {
            return Ok(TelemetryConfig::default());
        }
        let capture = MessageCapture::new(
            self.message_content_max_messages,
            self.message_content_max_bytes,
        )
        .map_err(|error| error.to_string())?;
        Ok(TelemetryConfig::default()
            .with_input_messages(capture)
            .with_output_messages(capture))
    }

    /// Appends fully resolved settings to a Kit child process. Empty endpoint
    /// and explicit false values prevent the child's environment or TOML from
    /// re-enabling export or message capture.
    pub fn append_cli_args(&self, command: &mut tokio::process::Command) {
        command
            .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
            .env_remove("OTEL_EXPORTER_OTLP_PROTOCOL")
            .env_remove("OTEL_EXPORTER_OTLP_TRACES_PROTOCOL")
            .arg("--otel-endpoint")
            .arg(self.endpoint.as_deref().unwrap_or_default())
            .arg("--otel-protocol")
            .arg(self.protocol.to_string())
            .arg("--otel-capture-message-content")
            .arg(self.capture_message_content.to_string())
            .arg("--otel-message-content-max-messages")
            .arg(self.message_content_max_messages.to_string())
            .arg("--otel-message-content-max-bytes")
            .arg(self.message_content_max_bytes.to_string());
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            endpoint: None,
            protocol: Protocol::default(),
            capture_message_content: false,
            message_content_max_messages: DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES,
            message_content_max_bytes: DEFAULT_MESSAGE_CONTENT_MAX_BYTES,
        }
    }
}

fn http_trace_endpoint(endpoint: &str) -> Result<String, String> {
    let parsed = reqwest::Url::parse(endpoint)
        .map_err(|error| format!("invalid OTLP HTTP trace endpoint {endpoint:?}: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(format!(
            "invalid OTLP HTTP trace endpoint {endpoint:?}: expected an http or https URL with a host"
        ));
    }
    if parsed.fragment().is_some() {
        return Err(format!(
            "invalid OTLP HTTP trace endpoint {endpoint:?}: URL fragments are not supported"
        ));
    }

    let normalized = parsed.as_str();
    let (base, query) = normalized
        .split_once('?')
        .map_or((normalized, None), |(base, query)| (base, Some(query)));
    let base = base.trim_end_matches('/');
    let path = parsed.path().trim_end_matches('/');
    let suffix = if path.ends_with("/v1/traces") {
        ""
    } else {
        "/v1/traces"
    };
    Ok(match query {
        Some(query) => format!("{base}{suffix}?{query}"),
        None => format!("{base}{suffix}"),
    })
}

const PROVIDER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

fn shutdown_provider_with_timeout(
    provider: opentelemetry_sdk::trace::SdkTracerProvider,
    timeout: Duration,
) -> Result<(), String> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let shutdown = thread::Builder::new()
        .name("kit-otel-shutdown".into())
        .spawn(move || {
            let _ = sender.send(provider.shutdown_with_timeout(timeout));
        })
        .map_err(|error| format!("could not start OpenTelemetry shutdown: {error}"))?;

    match receiver.recv_timeout(timeout) {
        Ok(result) => {
            shutdown
                .join()
                .map_err(|_| "OpenTelemetry shutdown thread panicked".to_string())?;
            result.map_err(|error| error.to_string())
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "OpenTelemetry shutdown timed out after {timeout:?}"
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let _ = shutdown.join();
            Err("OpenTelemetry shutdown thread disconnected".into())
        }
    }
}

/// Keeps the tracer provider alive and flushes queued spans when Kit exits.
pub struct Guard {
    provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    protocol: Protocol,
}

impl Drop for Guard {
    fn drop(&mut self) {
        let Some(provider) = self.provider.take() else {
            return;
        };
        let result = match self.protocol {
            Protocol::Grpc => provider.shutdown().map_err(|error| error.to_string()),
            Protocol::HttpProtobuf | Protocol::HttpJson => {
                shutdown_provider_with_timeout(provider, PROVIDER_SHUTDOWN_TIMEOUT)
            }
        };
        if let Err(error) = result {
            eprintln!("could not shut down OpenTelemetry exporter: {error}");
        }
    }
}

fn exported_targets() -> Targets {
    Targets::new()
        .with_target("agentkit_loop", LevelFilter::INFO)
        .with_target("agentkit_mcp", LevelFilter::INFO)
}

fn build_exporter(
    endpoint: &str,
    protocol: Protocol,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    // Leave headers unset so the upstream exporter applies its standard generic
    // and trace-specific header environment variables.
    match protocol {
        Protocol::Grpc => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_protocol(OtlpProtocol::Grpc)
            .with_endpoint(endpoint)
            .build(),
        Protocol::HttpProtobuf => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(OtlpProtocol::HttpBinary)
            .with_endpoint(endpoint)
            .build(),
        Protocol::HttpJson => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_protocol(OtlpProtocol::HttpJson)
            .with_endpoint(endpoint)
            .build(),
    }
}

fn build_provider(
    exporter: opentelemetry_otlp::SpanExporter,
    protocol: Protocol,
) -> opentelemetry_sdk::trace::SdkTracerProvider {
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();
    let builder = opentelemetry_sdk::trace::SdkTracerProvider::builder().with_resource(resource);
    match protocol {
        Protocol::Grpc => builder.with_batch_exporter(exporter).build(),
        Protocol::HttpProtobuf | Protocol::HttpJson => {
            let processor = opentelemetry_sdk::trace::span_processor_with_async_runtime::BatchSpanProcessor::builder(
                exporter,
                opentelemetry_sdk::runtime::Tokio,
            )
            .build();
            builder.with_span_processor(processor).build()
        }
    }
}

/// Installs OTLP trace export when an endpoint is configured.
pub fn init(settings: &Settings) -> Result<Option<Guard>, Box<dyn std::error::Error>> {
    let Some(endpoint) = settings.endpoint.as_deref() else {
        return Ok(None);
    };
    let exporter = build_exporter(endpoint, settings.protocol)?;
    let provider = build_provider(exporter, settings.protocol);
    let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_location(false)
        .with_threads(false)
        .with_tracked_inactivity(false)
        .with_target(false)
        .with_filter(exported_targets());
    tracing_subscriber::registry().with(layer).try_init()?;
    Ok(Some(Guard {
        provider: Some(provider),
        protocol: settings.protocol,
    }))
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read as _, Write as _},
        net::TcpListener,
        thread,
        time::{Duration, Instant},
    };

    use opentelemetry::trace::{Span as _, Tracer as _, TracerProvider as _};

    use super::{
        DEFAULT_MESSAGE_CONTENT_MAX_BYTES, DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES, LevelFilter,
        Protocol, Settings, build_exporter, build_provider, exported_targets,
        shutdown_provider_with_timeout,
    };

    #[test]
    fn settings_are_bounded_and_default_to_capture_off() {
        let defaults = Settings::default();
        assert_eq!(defaults.protocol, Protocol::Grpc);
        assert!(!defaults.capture_message_content);
        let config = defaults.agentkit_config().unwrap();
        assert_eq!(config.input_messages(), None);
        assert_eq!(config.output_messages(), None);
        assert_eq!(
            defaults.message_content_max_messages,
            DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES
        );
        assert_eq!(
            defaults.message_content_max_bytes,
            DEFAULT_MESSAGE_CONTENT_MAX_BYTES
        );
        assert!(Settings::try_new(None, true, 0, 1).is_err());
        assert!(Settings::try_new(None, true, 1, 0).is_err());
        assert!(Settings::try_new(None, true, super::MAX_MESSAGE_CONTENT_MESSAGES + 1, 1).is_err());
        assert!(Settings::try_new(None, true, 1, super::MAX_MESSAGE_CONTENT_BYTES + 1).is_err());
    }

    #[test]
    fn protocol_parsing_and_display_use_standard_values() {
        for (value, protocol) in [
            ("grpc", Protocol::Grpc),
            ("http/protobuf", Protocol::HttpProtobuf),
            ("http/json", Protocol::HttpJson),
        ] {
            assert_eq!(value.parse::<Protocol>().unwrap(), protocol);
            assert_eq!(protocol.to_string(), value);
        }
        let error = "http".parse::<Protocol>().unwrap_err();
        assert!(error.contains("grpc, http/protobuf, or http/json"));
        assert!("HTTP/JSON".parse::<Protocol>().is_err());
    }

    #[test]
    fn http_endpoints_gain_one_trace_suffix_and_preserve_queries() {
        for (endpoint, expected) in [
            (
                "https://otel.example.com/ingest",
                "https://otel.example.com/ingest/v1/traces",
            ),
            (
                "https://otel.example.com/ingest/",
                "https://otel.example.com/ingest/v1/traces",
            ),
            (
                "https://otel.example.com/ingest/v1/traces",
                "https://otel.example.com/ingest/v1/traces",
            ),
            (
                "https://otel.example.com/ingest?token=a%2Fb",
                "https://otel.example.com/ingest/v1/traces?token=a%2Fb",
            ),
        ] {
            let settings = Settings::try_new_with_protocol(
                Some(endpoint.into()),
                Protocol::HttpProtobuf,
                false,
                12,
                4096,
            )
            .unwrap();
            assert_eq!(settings.endpoint.as_deref(), Some(expected));
        }
    }

    #[test]
    fn invalid_http_endpoints_are_rejected_clearly() {
        for endpoint in [
            "collector:4318",
            "ftp://collector:4318",
            "https://collector/#part",
        ] {
            let error = Settings::try_new_with_protocol(
                Some(endpoint.into()),
                Protocol::HttpJson,
                false,
                12,
                4096,
            )
            .unwrap_err();
            assert!(
                error.contains("invalid OTLP HTTP trace endpoint"),
                "{error}"
            );
        }
    }

    fn serve_otlp_response(
        content_type: &'static str,
        body: &'static [u8],
        delay: Duration,
    ) -> (String, thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = vec![0; 8192];
            let length = stream.read(&mut request).unwrap();
            request.truncate(length);
            thread::sleep(delay);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
            request
        });
        (format!("http://{address}"), server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn http_transports_export_span_batches() {
        for (protocol, content_type, response_body) in [
            (Protocol::HttpProtobuf, "application/x-protobuf", &[][..]),
            (Protocol::HttpJson, "application/json", &b"{}"[..]),
        ] {
            let (endpoint, server) =
                serve_otlp_response(content_type, response_body, Duration::ZERO);
            let settings =
                Settings::try_new_with_protocol(Some(endpoint), protocol, false, 12, 4096).unwrap();
            let exporter = build_exporter(settings.endpoint.as_deref().unwrap(), protocol).unwrap();
            let provider = build_provider(exporter, protocol);
            let tracer = provider.tracer("http-export-test");
            let mut span = tracer.start("test span");
            span.end();
            provider.shutdown().unwrap();

            let request = server.join().unwrap();
            let header_end = request
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let request = String::from_utf8(request[..header_end].to_vec())
                .unwrap()
                .to_ascii_lowercase();
            assert!(request.starts_with("post /v1/traces http/1.1"), "{request}");
            assert!(
                request.contains(&format!("content-type: {content_type}")),
                "{request}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stalled_http_export_does_not_exceed_shutdown_deadline() {
        let delay = Duration::from_secs(1);
        let timeout = Duration::from_millis(50);
        let (endpoint, server) = serve_otlp_response("application/json", b"{}", delay);
        let settings =
            Settings::try_new_with_protocol(Some(endpoint), Protocol::HttpJson, false, 12, 4096)
                .unwrap();
        let exporter =
            build_exporter(settings.endpoint.as_deref().unwrap(), Protocol::HttpJson).unwrap();
        let provider = build_provider(exporter, Protocol::HttpJson);
        let tracer = provider.tracer("stalled-http-export-test");
        let mut span = tracer.start("test span");
        span.end();

        let started = Instant::now();
        let error = shutdown_provider_with_timeout(provider, timeout).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
        assert!(started.elapsed() < delay);
        server.join().unwrap();
    }

    #[test]
    fn enabled_capture_maps_validated_limits_to_input_and_output() {
        let settings = Settings::try_new(None, true, 12, 4096).unwrap();
        let config = settings.agentkit_config().unwrap();
        let input = config.input_messages().unwrap();
        let output = config.output_messages().unwrap();
        assert_eq!(input.max_messages(), 12);
        assert_eq!(input.max_bytes(), 4096);
        assert_eq!(output, input);

        let invalid = Settings {
            message_content_max_messages: 0,
            ..Settings::default()
        };
        assert!(invalid.agentkit_config().is_err());
    }

    #[test]
    fn child_args_propagate_protocol_endpoint_explicit_false_and_bounds() {
        let settings = Settings::try_new_with_protocol(
            Some("http://collector:4318".into()),
            Protocol::HttpJson,
            false,
            12,
            4096,
        )
        .unwrap();
        let mut command = tokio::process::Command::new("kit");
        settings.append_cli_args(&mut command);
        for expected in [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_PROTOCOL",
            "OTEL_EXPORTER_OTLP_TRACES_PROTOCOL",
        ] {
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .any(|(name, value)| { name == expected && value.is_none() })
            );
        }
        for inherited in [
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_EXPORTER_OTLP_TRACES_HEADERS",
        ] {
            assert!(
                command
                    .as_std()
                    .get_envs()
                    .all(|(name, _)| name != inherited)
            );
        }
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--otel-endpoint",
                "http://collector:4318/v1/traces",
                "--otel-protocol",
                "http/json",
                "--otel-capture-message-content",
                "false",
                "--otel-message-content-max-messages",
                "12",
                "--otel-message-content-max-bytes",
                "4096",
            ]
        );
    }

    #[test]
    fn disabled_endpoint_is_explicitly_propagated_to_children() {
        let settings = Settings::default();
        let mut command = tokio::process::Command::new("kit");
        settings.append_cli_args(&mut command);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            &args[..4],
            ["--otel-endpoint", "", "--otel-protocol", "grpc"]
        );
        assert!(
            args.windows(2)
                .any(|pair| { pair == ["--otel-capture-message-content", "false"] })
        );
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(name, value)| { name == "OTEL_EXPORTER_OTLP_ENDPOINT" && value.is_none() })
        );
    }

    #[test]
    fn exporter_allows_semantic_targets_and_rejects_dependencies() {
        let targets: Vec<_> = exported_targets().into_iter().collect();
        assert_eq!(
            targets,
            vec![
                ("agentkit_loop".to_string(), LevelFilter::INFO),
                ("agentkit_mcp".to_string(), LevelFilter::INFO),
            ]
        );
    }
}
