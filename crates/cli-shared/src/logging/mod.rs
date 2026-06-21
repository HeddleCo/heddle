// SPDX-License-Identifier: Apache-2.0
//! Structured logging initialization and configuration.
//!
//! This module provides centralized logging setup using the `tracing` ecosystem.
//! It supports both human-readable and JSON output formats, configurable via
//! environment variables.
//!
//! # Configuration
//!
//! Logging is controlled via the `RUST_LOG` environment variable:
//!
//! ```bash
//! # Default logging (info level)
//! RUST_LOG=info
//!
//! # Debug level for heddle only
//! RUST_LOG=heddle=debug
//!
//! # Trace everything
//! RUST_LOG=trace
//!
//! # JSON output for machine parsing
//! RUST_LOG=info HEDDLE_LOG_FORMAT=json
//! ```

use std::io::{self, IsTerminal};

#[cfg(feature = "observability")]
use opentelemetry::{KeyValue, global, trace::TracerProvider as _};
#[cfg(feature = "observability")]
use opentelemetry_otlp::WithExportConfig;
#[cfg(feature = "observability")]
use opentelemetry_sdk::{Resource, metrics::SdkMeterProvider, trace::SdkTracerProvider};
use tracing::Level;
use tracing_subscriber::{
    EnvFilter, fmt::format::FmtSpan, layer::SubscriberExt, util::SubscriberInitExt,
};

use crate::config::UserConfig;

fn is_truthy(val: &str) -> bool {
    matches!(
        val.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub format: LogFormat,
    /// Filter level used when `RUST_LOG` is unset. Foreground CLI commands
    /// default this to `Warn`; `-v` raises to Info, `-vv` to Debug, `-vvv`
    /// to Trace; `--quiet` lowers to Error. `RUST_LOG` always overrides.
    pub default_level: Level,
    pub include_location: bool,
    pub include_thread_ids: bool,
    pub log_spans: bool,
    pub otel_service_name: Option<String>,
    pub otel_endpoint: Option<String>,
    pub otel_traces_endpoint: Option<String>,
    pub otel_metrics_endpoint: Option<String>,
}

#[derive(Debug, Default)]
pub struct LoggingGuard {
    #[cfg(feature = "observability")]
    tracer_provider: Option<SdkTracerProvider>,
    #[cfg(feature = "observability")]
    meter_provider: Option<SdkMeterProvider>,
}

impl LoggingGuard {
    pub fn shutdown(self) {
        #[cfg(feature = "observability")]
        {
            if let Some(meter_provider) = self.meter_provider {
                let _ = meter_provider.shutdown();
            }
            if let Some(tracer_provider) = self.tracer_provider {
                let _ = tracer_provider.shutdown();
            }
        }
    }
}

#[cfg(feature = "observability")]
#[derive(Debug, Clone)]
struct OtelConfig {
    service_name: String,
    trace_endpoint: Option<String>,
    metrics_endpoint: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            format: LogFormat::Text,
            default_level: Level::WARN,
            include_location: false,
            include_thread_ids: false,
            log_spans: false,
            otel_service_name: None,
            otel_endpoint: None,
            otel_traces_endpoint: None,
            otel_metrics_endpoint: None,
        }
    }
}

impl LoggingConfig {
    pub fn from_env() -> Self {
        Self::from_user_and_env(None)
    }

    pub fn from_user_and_env(user_config: Option<&UserConfig>) -> Self {
        let mut config = Self::default();

        if let Some(user_config) = user_config {
            if user_config
                .logging
                .format
                .as_deref()
                .is_some_and(|format| format.eq_ignore_ascii_case("json"))
            {
                config.format = LogFormat::Json;
            }
            config.include_location = user_config.logging.include_location;
            config.include_thread_ids = user_config.logging.include_thread_ids;
            config.log_spans = user_config.logging.log_spans;
            config.otel_service_name = user_config.logging.otel_service_name.clone();
            config.otel_endpoint = user_config.logging.otel_endpoint.clone();
            config.otel_traces_endpoint = user_config.logging.otel_traces_endpoint.clone();
            config.otel_metrics_endpoint = user_config.logging.otel_metrics_endpoint.clone();
        }

        if let Ok(format) = std::env::var("HEDDLE_LOG_FORMAT")
            && format.eq_ignore_ascii_case("json")
        {
            config.format = LogFormat::Json;
        }

        if std::env::var("HEDDLE_LOG_LOCATION")
            .map(|v| is_truthy(&v))
            .unwrap_or(false)
        {
            config.include_location = true;
        }

        if std::env::var("HEDDLE_LOG_THREADS")
            .map(|v| is_truthy(&v))
            .unwrap_or(false)
        {
            config.include_thread_ids = true;
        }

        if std::env::var("HEDDLE_LOG_SPANS")
            .map(|v| is_truthy(&v))
            .unwrap_or(false)
        {
            config.log_spans = true;
        }

        if let Ok(service_name) = std::env::var("OTEL_SERVICE_NAME") {
            config.otel_service_name = Some(service_name);
        }
        if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT") {
            config.otel_endpoint = Some(endpoint);
        }
        if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT") {
            config.otel_traces_endpoint = Some(endpoint);
        }
        if let Ok(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT") {
            config.otel_metrics_endpoint = Some(endpoint);
        }

        config
    }

    pub fn with_format(mut self, format: LogFormat) -> Self {
        self.format = format;
        self
    }

    /// Map CLI `-v`/`--quiet` counts to a default log level.
    ///
    /// `quiet` wins over `verbose`; `RUST_LOG` overrides both downstream.
    /// 0 → keep current (e.g. `Warn` for foreground), 1 → Info, 2 → Debug,
    /// 3+ → Trace.
    pub fn with_verbosity(mut self, verbose: u8, quiet: bool) -> Self {
        self.default_level = if quiet {
            Level::ERROR
        } else {
            match verbose {
                0 => self.default_level,
                1 => Level::INFO,
                2 => Level::DEBUG,
                _ => Level::TRACE,
            }
        };
        self
    }

    pub fn with_location(mut self, include: bool) -> Self {
        self.include_location = include;
        self
    }

    pub fn with_thread_ids(mut self, include: bool) -> Self {
        self.include_thread_ids = include;
        self
    }

    pub fn with_spans(mut self, include: bool) -> Self {
        self.log_spans = include;
        self
    }
}

#[cfg(feature = "observability")]
impl OtelConfig {
    fn from_logging_config(config: &LoggingConfig) -> Self {
        let shared_endpoint = config.otel_endpoint.clone();
        Self {
            service_name: config
                .otel_service_name
                .clone()
                .unwrap_or_else(|| "heddle".to_string()),
            trace_endpoint: config
                .otel_traces_endpoint
                .clone()
                .or_else(|| shared_endpoint.clone()),
            metrics_endpoint: config.otel_metrics_endpoint.clone().or(shared_endpoint),
        }
    }

    fn enabled(&self) -> bool {
        self.trace_endpoint.is_some() || self.metrics_endpoint.is_some()
    }

    #[cfg(feature = "observability")]
    fn resource(&self) -> Resource {
        Resource::builder_empty()
            .with_attributes([KeyValue::new("service.name", self.service_name.clone())])
            .build()
    }
}

/// Initialize the global tracing subscriber.
///
/// # Example
///
/// ```rust
/// use cli_shared::logging::{LoggingConfig, init_logging};
///
/// fn main() {
///     init_logging(LoggingConfig::from_env());
///
///     tracing::info!("Logging initialized");
/// }
/// ```
pub fn init_logging(config: LoggingConfig) -> LoggingGuard {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level_to_filter(config.default_level)));
    let span_events = if config.log_spans {
        FmtSpan::FULL
    } else {
        FmtSpan::NONE
    };
    let telemetry = init_otel(&config);
    let registry = tracing_subscriber::registry().with(env_filter);

    #[cfg(feature = "observability")]
    let init_result = match (config.format, telemetry.tracer_provider.as_ref()) {
        (LogFormat::Text, Some(provider)) => registry
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer(telemetry.service_name.clone())),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(config.include_thread_ids)
                    .with_file(config.include_location)
                    .with_line_number(config.include_location)
                    .with_span_events(span_events)
                    .with_ansi(io::stderr().is_terminal()),
            )
            .try_init(),
        (LogFormat::Text, None) => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(config.include_thread_ids)
                    .with_file(config.include_location)
                    .with_line_number(config.include_location)
                    .with_span_events(span_events)
                    .with_ansi(io::stderr().is_terminal()),
            )
            .try_init(),
        (LogFormat::Json, Some(provider)) => registry
            .with(
                tracing_opentelemetry::layer()
                    .with_tracer(provider.tracer(telemetry.service_name.clone())),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(config.include_thread_ids)
                    .with_file(config.include_location)
                    .with_line_number(config.include_location)
                    .with_span_events(span_events),
            )
            .try_init(),
        (LogFormat::Json, None) => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(config.include_thread_ids)
                    .with_file(config.include_location)
                    .with_line_number(config.include_location)
                    .with_span_events(span_events),
            )
            .try_init(),
    };

    #[cfg(not(feature = "observability"))]
    let init_result = match config.format {
        LogFormat::Text => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(config.include_thread_ids)
                    .with_file(config.include_location)
                    .with_line_number(config.include_location)
                    .with_span_events(span_events)
                    .with_ansi(io::stderr().is_terminal()),
            )
            .try_init(),
        LogFormat::Json => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .with_writer(io::stderr)
                    .with_target(true)
                    .with_level(true)
                    .with_thread_ids(config.include_thread_ids)
                    .with_file(config.include_location)
                    .with_line_number(config.include_location)
                    .with_span_events(span_events),
            )
            .try_init(),
    };

    if let Err(err) = init_result {
        eprintln!("failed to initialize tracing subscriber: {err}");
    }

    telemetry.guard
}

pub fn init_logging_default() {
    let _ = init_logging(LoggingConfig::default());
}

fn level_to_filter(level: Level) -> &'static str {
    match level {
        Level::TRACE => "trace",
        Level::DEBUG => "debug",
        Level::INFO => "info",
        Level::WARN => "warn",
        Level::ERROR => "error",
    }
}

pub fn is_enabled(level: Level) -> bool {
    tracing::level_enabled!(level)
}

#[macro_export]
macro_rules! log_operation {
    ($operation:expr, $($key:ident = $value:expr),+ $(,)?) => {
        tracing::info!(
            operation = %$operation,
            $($key = %$value),+,
            "Operation executed"
        )
    };
    ($operation:expr) => {
        tracing::info!(operation = %$operation, "Operation executed")
    };
}

#[macro_export]
macro_rules! log_repo_event {
    ($event:expr, change_id = $change_id:expr $(, $key:ident = $value:expr)* $(,)?) => {
        tracing::info!(
            event = %$event,
            change_id = %$change_id,
            $($key = %$value),*,
            "Repository event"
        )
    };
}

struct TelemetryInit {
    guard: LoggingGuard,
    #[cfg(feature = "observability")]
    tracer_provider: Option<SdkTracerProvider>,
    #[cfg(feature = "observability")]
    service_name: String,
}

#[cfg(feature = "observability")]
fn init_otel(logging: &LoggingConfig) -> TelemetryInit {
    let config = OtelConfig::from_logging_config(logging);
    if !config.enabled() {
        return TelemetryInit {
            guard: LoggingGuard::default(),
            tracer_provider: None,
            service_name: config.service_name,
        };
    }

    let resource = config.resource();
    let tracer_provider = config.trace_endpoint.as_ref().and_then(|endpoint| {
        let exporter = opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.to_string())
            .build()
            .map_err(|err| {
                eprintln!("failed to initialize OTLP trace exporter: {err}");
                err
            })
            .ok()?;
        let provider = SdkTracerProvider::builder()
            .with_resource(resource.clone())
            .with_batch_exporter(exporter)
            .build();
        global::set_tracer_provider(provider.clone());
        Some(provider)
    });

    let meter_provider = config.metrics_endpoint.as_ref().and_then(|endpoint| {
        let exporter = opentelemetry_otlp::MetricExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint.to_string())
            .build()
            .map_err(|err| {
                eprintln!("failed to initialize OTLP metric exporter: {err}");
                err
            })
            .ok()?;
        let provider = SdkMeterProvider::builder()
            .with_periodic_exporter(exporter)
            .with_resource(resource.clone())
            .build();
        global::set_meter_provider(provider.clone());
        Some(provider)
    });

    TelemetryInit {
        guard: LoggingGuard {
            tracer_provider: tracer_provider.clone(),
            meter_provider,
        },
        tracer_provider,
        service_name: config.service_name,
    }
}

#[cfg(not(feature = "observability"))]
fn init_otel(_logging: &LoggingConfig) -> TelemetryInit {
    TelemetryInit {
        guard: LoggingGuard::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logging_config_default() {
        let config = LoggingConfig::default();
        assert_eq!(config.format, LogFormat::Text);
        assert!(!config.include_location);
        assert!(!config.include_thread_ids);
        assert!(!config.log_spans);
    }

    #[test]
    fn test_logging_config_builder() {
        let config = LoggingConfig::default()
            .with_format(LogFormat::Json)
            .with_location(true)
            .with_thread_ids(true)
            .with_spans(true);

        assert_eq!(config.format, LogFormat::Json);
        assert!(config.include_location);
        assert!(config.include_thread_ids);
        assert!(config.log_spans);
    }

    #[test]
    fn test_is_truthy() {
        assert!(is_truthy("1"));
        assert!(is_truthy("true"));
        assert!(is_truthy("TRUE"));
        assert!(is_truthy("True"));
        assert!(is_truthy("yes"));
        assert!(is_truthy("YES"));
        assert!(is_truthy("on"));
        assert!(is_truthy("ON"));

        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy("FALSE"));
        assert!(!is_truthy("no"));
        assert!(!is_truthy("off"));
        assert!(!is_truthy(""));
        assert!(!is_truthy("random"));
    }
}
