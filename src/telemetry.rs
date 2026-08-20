//! Optional OpenTelemetry trace export and resolved host settings.

use agentkit_loop::{MessageCapture, TelemetryConfig};
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use tracing_subscriber::{
    Layer as _,
    filter::{LevelFilter, Targets},
    prelude::*,
};

pub const DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES: usize = 64;
pub const DEFAULT_MESSAGE_CONTENT_MAX_BYTES: usize = 16_384;
pub const MAX_MESSAGE_CONTENT_MESSAGES: usize = 1_024;
pub const MAX_MESSAGE_CONTENT_BYTES: usize = 1_048_576;

/// Fully resolved telemetry settings. Kit owns environment, TOML, and CLI
/// resolution; AgentKit receives no process environment configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub endpoint: Option<String>,
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
        Ok(Self {
            endpoint: endpoint.filter(|value| !value.is_empty()),
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
            .arg("--otel-endpoint")
            .arg(self.endpoint.as_deref().unwrap_or_default())
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
            capture_message_content: false,
            message_content_max_messages: DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES,
            message_content_max_bytes: DEFAULT_MESSAGE_CONTENT_MAX_BYTES,
        }
    }
}

/// Keeps the tracer provider alive and flushes queued spans when Kit exits.
pub struct Guard(opentelemetry_sdk::trace::SdkTracerProvider);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.0.shutdown() {
            eprintln!("could not shut down OpenTelemetry exporter: {error}");
        }
    }
}

fn exported_targets() -> Targets {
    Targets::new()
        .with_target("agentkit_loop", LevelFilter::INFO)
        .with_target("agentkit_mcp", LevelFilter::INFO)
}

/// Installs OTLP/gRPC trace export when an endpoint is configured.
pub fn init(settings: &Settings) -> Result<Option<Guard>, Box<dyn std::error::Error>> {
    let Some(endpoint) = settings.endpoint.as_deref() else {
        return Ok(None);
    };
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;
    let resource = opentelemetry_sdk::Resource::builder()
        .with_service_name(env!("CARGO_PKG_NAME"))
        .build();
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_resource(resource)
        .with_batch_exporter(exporter)
        .build();
    let tracer = provider.tracer(env!("CARGO_PKG_NAME"));
    let layer = tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_location(false)
        .with_threads(false)
        .with_tracked_inactivity(false)
        .with_target(false)
        .with_filter(exported_targets());
    tracing_subscriber::registry().with(layer).try_init()?;
    Ok(Some(Guard(provider)))
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_MESSAGE_CONTENT_MAX_BYTES, DEFAULT_MESSAGE_CONTENT_MAX_MESSAGES, LevelFilter,
        Settings, exported_targets,
    };

    #[test]
    fn settings_are_bounded_and_default_to_capture_off() {
        let defaults = Settings::default();
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
    fn child_args_propagate_endpoint_explicit_false_and_bounds() {
        let settings =
            Settings::try_new(Some("http://collector:4317".into()), false, 12, 4096).unwrap();
        let mut command = tokio::process::Command::new("kit");
        settings.append_cli_args(&mut command);
        assert!(
            command
                .as_std()
                .get_envs()
                .any(|(name, value)| { name == "OTEL_EXPORTER_OTLP_ENDPOINT" && value.is_none() })
        );
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--otel-endpoint",
                "http://collector:4317",
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

        assert_eq!(&args[..2], ["--otel-endpoint", ""]);
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
