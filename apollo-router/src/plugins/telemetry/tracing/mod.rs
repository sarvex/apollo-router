use std::fmt::Display;
use std::fmt::Formatter;
use std::time::Duration;

use opentelemetry::sdk::export::trace::SpanData;
use opentelemetry::sdk::trace::BatchConfig;
use opentelemetry::sdk::trace::Builder;
use opentelemetry::sdk::trace::EvictedHashMap;
use opentelemetry::sdk::trace::Span;
use opentelemetry::sdk::trace::SpanProcessor;
use opentelemetry::trace::TraceResult;
use opentelemetry::Context;
use opentelemetry::KeyValue;
use reqwest::Url;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde_json::Value;
use tower::BoxError;
use url::ParseError;

use crate::plugins::telemetry::config::Trace;

pub(crate) mod apollo;
pub(crate) mod apollo_telemetry;
pub(crate) mod datadog;
pub(crate) mod jaeger;
pub(crate) mod otlp;
pub(crate) mod zipkin;

pub(crate) trait TracingConfigurator {
    fn apply(&self, builder: Builder, trace_config: &Trace) -> Result<Builder, BoxError>;
}

schemar_fn!(
    agent_endpoint,
    String,
    Some(Value::String("default".to_string())),
    "The agent endpoint to send reports to"
);
/// The endpoint to send reports to
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case", untagged)]
pub(crate) enum AgentEndpoint {
    /// The default agent endpoint
    Default(AgentDefault),
    /// A custom URL endpoint
    Url(Url),
}

/// The default agent endpoint
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub(crate) enum AgentDefault {
    /// The default agent endpoint
    Default,
}

pub(crate) fn parse_url_for_endpoint(mut s: String) -> Result<Url, ParseError> {
    match Url::parse(&s) {
        Ok(url) => {
            // support the case of 'collector:4317' where url parses 'collector'
            // as the scheme instead of the host
            if url.host().is_none() && (url.scheme() != "http" || url.scheme() != "https") {
                s = format!("http://{s}");
                Url::parse(&s)
            } else {
                Ok(url)
            }
        }
        Err(err) => {
            match err {
                // support the case of '127.0.0.1:4317' where url is interpreted
                // as a relative url without a base
                ParseError::RelativeUrlWithoutBase => {
                    s = format!("http://{s}");
                    Url::parse(&s)
                }
                _ => Err(err),
            }
        }
    }
}

pub(crate) fn deser_endpoint<'de, D>(deserializer: D) -> Result<AgentEndpoint, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if s == "default" {
        return Ok(AgentEndpoint::Default(AgentDefault::Default));
    }
    let url = parse_url_for_endpoint(s).map_err(serde::de::Error::custom)?;
    Ok(AgentEndpoint::Url(url))
}

#[derive(Debug)]
struct ApolloFilterSpanProcessor<T: SpanProcessor> {
    delegate: T,
}

pub(crate) static APOLLO_PRIVATE_PREFIX: &str = "apollo_private.";

impl<T: SpanProcessor> SpanProcessor for ApolloFilterSpanProcessor<T> {
    fn on_start(&self, span: &mut Span, cx: &Context) {
        self.delegate.on_start(span, cx);
    }

    fn on_end(&self, span: SpanData) {
        if span
            .attributes
            .iter()
            .any(|(key, _)| key.as_str().starts_with(APOLLO_PRIVATE_PREFIX))
        {
            let attributes_len = span.attributes.len();
            let span = SpanData {
                attributes: span
                    .attributes
                    .into_iter()
                    .filter(|(k, _)| !k.as_str().starts_with(APOLLO_PRIVATE_PREFIX))
                    .fold(
                        EvictedHashMap::new(attributes_len as u32, attributes_len),
                        |mut m, (k, v)| {
                            m.insert(KeyValue::new(k, v));
                            m
                        },
                    ),
                ..span
            };

            self.delegate.on_end(span);
        } else {
            self.delegate.on_end(span);
        }
    }

    fn force_flush(&self) -> TraceResult<()> {
        self.delegate.force_flush()
    }

    fn shutdown(&mut self) -> TraceResult<()> {
        self.delegate.shutdown()
    }
}

trait SpanProcessorExt
where
    Self: Sized + SpanProcessor,
{
    fn filtered(self) -> ApolloFilterSpanProcessor<Self>;
}

impl<T: SpanProcessor> SpanProcessorExt for T
where
    Self: Sized,
{
    fn filtered(self) -> ApolloFilterSpanProcessor<Self> {
        ApolloFilterSpanProcessor { delegate: self }
    }
}

/// Batch processor configuration
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(default)]
pub(crate) struct BatchProcessorConfig {
    #[serde(deserialize_with = "humantime_serde::deserialize")]
    #[schemars(with = "String")]
    /// The delay interval in milliseconds between two consecutive processing
    /// of batches. The default value is 5 seconds.
    pub(crate) scheduled_delay: Duration,

    /// The maximum queue size to buffer spans for delayed processing. If the
    /// queue gets full it drops the spans. The default value of is 2048.
    pub(crate) max_queue_size: usize,

    /// The maximum number of spans to process in a single batch. If there are
    /// more than one batch worth of spans then it processes multiple batches
    /// of spans one batch after the other without any delay. The default value
    /// is 512.
    pub(crate) max_export_batch_size: usize,

    #[serde(deserialize_with = "humantime_serde::deserialize")]
    #[schemars(with = "String")]
    /// The maximum duration to export a batch of data.
    /// The default value is 30 seconds.
    pub(crate) max_export_timeout: Duration,

    /// Maximum number of concurrent exports
    ///
    /// Limits the number of spawned tasks for exports and thus memory consumed
    /// by an exporter. A value of 1 will cause exports to be performed
    /// synchronously on the BatchSpanProcessor task.
    /// The default is 1.
    pub(crate) max_concurrent_exports: usize,
}

fn scheduled_delay_default() -> Duration {
    Duration::from_secs(5)
}

fn max_queue_size_default() -> usize {
    2048
}

fn max_export_batch_size_default() -> usize {
    512
}

fn max_export_timeout_default() -> Duration {
    Duration::from_secs(30)
}

fn max_concurrent_exports_default() -> usize {
    1
}

impl From<BatchProcessorConfig> for BatchConfig {
    fn from(config: BatchProcessorConfig) -> Self {
        let mut default = BatchConfig::default();
        default = default.with_scheduled_delay(config.scheduled_delay);
        default = default.with_max_queue_size(config.max_queue_size);
        default = default.with_max_export_batch_size(config.max_export_batch_size);
        default = default.with_max_export_timeout(config.max_export_timeout);
        default = default.with_max_concurrent_exports(config.max_concurrent_exports);
        default
    }
}

impl Display for BatchProcessorConfig {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format!("BatchConfig {{ scheduled_delay={}, max_queue_size={}, max_export_batch_size={}, max_export_timeout={}, max_concurrent_exports={} }}",
                             humantime::format_duration(self.scheduled_delay),
                             self.max_queue_size,
                             self.max_export_batch_size,
                             humantime::format_duration(self.max_export_timeout),
                             self.max_concurrent_exports))
    }
}

impl Default for BatchProcessorConfig {
    fn default() -> Self {
        BatchProcessorConfig {
            scheduled_delay: scheduled_delay_default(),
            max_queue_size: max_queue_size_default(),
            max_export_batch_size: max_export_batch_size_default(),
            max_export_timeout: max_export_timeout_default(),
            max_concurrent_exports: max_concurrent_exports_default(),
        }
    }
}
