//! This crate configures tracing; because we perform logging through the tracing subsystem,
//! logging is configured here as well.
//!
//! For most purposes, you can use the normal set of primitives from the [tracing] family of
//! crates, such as the [`#[instrument]`](tracing-attributes::instrument) macro, and simply allow
//! this crate to deal with configuration.
//!
//! Other than configuration, the functionality provided by this crate is primarily useful for
//! Performance-critical codepaths, such as the hotpath for queries in ReadySet and the adapter.
//!
//! # Performance-critical codepaths
//! For performance-critical pieces, the story is a bit more complex.  Because there is a
//! substantial performance cost involved with creating a [Span](tracing::Span) to begin with,
//! compared to a call to [Span::none()](tracing::Span::none), there are savings to be had by
//! [presampling](presampled) - sampling spans at creation time rather than when a subscriber would
//! send them to a collector.

#![feature(core_intrinsics)]
use clap::Parser;
use opentelemetry::sdk::trace::{Sampler, Tracer};
use opentelemetry::sdk::Resource;
use opentelemetry::KeyValue;
use opentelemetry_otlp::WithExportConfig;
use tracing::Subscriber;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::filter::ParseError;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{fmt, EnvFilter, Layer};

mod error;
pub use error::Error;
mod logformat;
use logformat::LogFormat;
mod percent;
use percent::Percent;
use tracing_wrapper::set_log_field;
pub mod presampled;
pub mod propagation;
pub mod tracing_wrapper;
#[macro_use]
mod macros;

pub fn warn_if_debug_build() {
    if cfg!(debug) {
        tracing::warn!("Running a debug build")
    }
}

#[derive(Debug, Parser)]
pub struct Options {
    /// Format to use when emitting log events.
    #[clap(
        long,
        env = "LOG_FORMAT",
        parse(try_from_str),
        default_value = "full",
        possible_values = &["compact", "full", "pretty", "json"]
    )]
    log_format: LogFormat,

    /// Log level filter for spans and events. The log level filter string is a comma separated
    /// list of directives.
    /// See [`tracing_subscriber::EnvFilter`] for full documentation on the directive syntax.
    ///
    /// Examples:
    ///
    /// Log at INFO level for all crates and dependencies.
    /// ```bash
    /// LOG_LEVEL=info
    /// ```
    ///
    /// Log at TRACE level for all crates and dependencies except
    /// tower which should be logged at ERROR level.
    /// ```bash
    /// LOG_LEVEL=trace,tower=error
    /// ```
    #[clap(long, env = "LOG_LEVEL", default_value = "info")]
    log_level: String,

    /// Host and port to send OTLP traces/spans data, via GRPC OLTP
    #[clap(long, env = "TRACING_HOST")]
    tracing_host: Option<String>,

    /// Portion of traces that will be sent to the tracing endpoint; [0.0~1.0]
    #[clap(long, env = "TRACING_SAMPLE_PERCENT", default_value_t = Percent(0.01))]
    tracing_sample_percent: Percent,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            log_format: LogFormat::Full,
            log_level: "info".to_owned(),
            tracing_host: None,
            tracing_sample_percent: Percent(0.01),
        }
    }
}

impl Options {
    fn tracing_layer<S>(
        &self,
        service_name: &str,
        deployment: &str,
    ) -> Result<OpenTelemetryLayer<S, Tracer>, Error>
    where
        S: Subscriber + for<'span> LookupSpan<'span>,
    {
        let resources = vec![
            KeyValue::new(
                opentelemetry_semantic_conventions::resource::SERVICE_NAME,
                service_name.to_owned(),
            ),
            KeyValue::new(
                opentelemetry_semantic_conventions::resource::SERVICE_NAMESPACE,
                deployment.to_owned(),
            ),
        ];

        let tracer = opentelemetry_otlp::new_pipeline()
            .tracing()
            .with_exporter(
                opentelemetry_otlp::new_exporter()
                    .tonic()
                    .with_endpoint(self.tracing_host.as_ref().unwrap()),
            )
            .with_trace_config(
                opentelemetry::sdk::trace::config()
                    .with_sampler(Sampler::TraceIdRatioBased(self.tracing_sample_percent.0))
                    .with_max_events_per_span(64)
                    .with_max_attributes_per_span(16)
                    .with_resource(Resource::new(resources)),
            )
            .install_batch(opentelemetry::runtime::Tokio)
            .unwrap();

        Ok(tracing_opentelemetry::layer().with_tracer(tracer))
    }

    // Anything we do with a templated type alias or a custom trait is just going to make this more
    // difficult to read/follow
    #[allow(clippy::type_complexity)]
    fn logging_layer<S>(&self) -> Result<Box<dyn Layer<S> + Send + Sync>, ParseError>
    where
        S: Subscriber + Send + Sync + for<'span> LookupSpan<'span>,
    {
        let layer: Box<dyn Layer<S> + Send + Sync> = match self.log_format {
            LogFormat::Compact => Box::new(fmt::layer().compact()),
            LogFormat::Full => Box::new(fmt::layer()),
            LogFormat::Pretty => Box::new(fmt::layer().pretty()),
            LogFormat::Json => Box::new(fmt::layer().json().with_current_span(true)),
        };
        Ok(layer)
    }

    fn init_logging_and_tracing(&self, service_name: &str, deployment: &str) -> Result<(), Error> {
        use tracing_subscriber::prelude::*;
        tracing_subscriber::registry()
            .with(tracing_subscriber::EnvFilter::new(&self.log_level))
            .with(self.tracing_layer(service_name, deployment)?)
            .with(self.logging_layer()?)
            .init();
        Ok(())
    }

    fn init_logging_only(&self) -> Result<(), ParseError> {
        let filter = EnvFilter::try_new(&self.log_level)?;
        let s = tracing_subscriber::fmt().with_env_filter(filter);

        match self.log_format {
            LogFormat::Compact => s.compact().init(),
            LogFormat::Full => s.init(),
            LogFormat::Pretty => s.pretty().init(),
            LogFormat::Json => s.json().with_current_span(true).init(),
        }

        #[cfg(debug)]
        warn_if_debug_build();

        Ok(())
    }

    /// This is the primary entrypoint to the combined logging/tracing subsystem.  If tracing is
    /// not configured, it will initialize logging with static dispatch for the format, saving the
    /// performance cost.
    ///
    /// When the `Options` struct itself goes out of scope, it will take care of the call to
    /// [opentelemetry::global::shutdown_tracer_provider] so that, when developing calling code,
    /// you don't need to remember.
    ///
    /// # Panics
    /// This will panic if called with tracing enabled outside the context of a tokio runtime.
    ///
    /// Example:
    /// ```
    /// use clap::Parser;
    ///
    /// #[derive(Debug, Parser)]
    /// struct Options {
    ///     #[clap(flatten)]
    ///     tracing: readyset_tracing::Options,
    /// }
    ///
    /// #[tokio::main]
    /// async fn main() {
    ///     let options = Options::parse();
    ///     options
    ///         .tracing
    ///         .init("tracing-example", "example-deplyoment")
    ///         .unwrap();
    ///
    ///     // Perform work!
    /// }
    /// ```
    pub fn init(&self, service_name: &str, deployment: &str) -> Result<(), Error> {
        set_log_field("deployment".into(), deployment.into());
        if self.tracing_host.is_some() {
            self.init_logging_and_tracing(service_name, deployment)
        } else {
            self.init_logging_only().map_err(|e| e.into())
        }
    }
}

impl Drop for Options {
    fn drop(&mut self) {
        if self.tracing_host.is_some() {
            opentelemetry::global::shutdown_tracer_provider()
        }
    }
}

/// Configure the global tracing subscriber for logging inside of tests
pub fn init_test_logging() {
    // This errors out if it's already been called within the scope of a process, which we don't
    // care about, so we just discard the result
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_env("LOG_LEVEL"))
        .with_test_writer()
        .try_init();
}
