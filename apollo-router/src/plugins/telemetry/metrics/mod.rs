use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use ::serde::Deserialize;
use access_json::JSONQuery;
use http::header::HeaderName;
use http::response::Parts;
use http::HeaderMap;
use multimap::MultiMap;
use opentelemetry::metrics::Counter;
use opentelemetry::metrics::Meter;
use opentelemetry::metrics::MeterProvider;
use opentelemetry::metrics::Number;
use opentelemetry::metrics::ValueRecorder;
use opentelemetry::KeyValue;
use regex::Regex;
use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use tower::BoxError;

use crate::error::FetchError;
use crate::graphql;
use crate::graphql::Request;
use crate::plugin::serde::deserialize_header_name;
use crate::plugin::serde::deserialize_json_query;
use crate::plugin::serde::deserialize_regex;
use crate::plugins::telemetry::apollo_exporter::Sender;
use crate::plugins::telemetry::config::MetricsCommon;
use crate::router_factory::Endpoint;
use crate::Context;
use crate::ListenAddr;

pub(crate) mod apollo;
pub(crate) mod otlp;
pub(crate) mod prometheus;

pub(crate) type MetricsExporterHandle = Box<dyn Any + Send + Sync + 'static>;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Configuration to add custom attributes/labels on metrics
pub(crate) struct MetricsAttributesConf {
    /// Configuration to forward header values or body values from router request/response in metric attributes/labels
    pub(crate) router: Option<AttributesForwardConf>,
    /// Configuration to forward header values or body values from subgraph request/response in metric attributes/labels
    pub(crate) subgraph: Option<SubgraphAttributesConf>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubgraphAttributesConf {
    // Apply to all subgraphs
    pub(crate) all: Option<AttributesForwardConf>,
    // Apply to specific subgraph
    pub(crate) subgraphs: Option<HashMap<String, AttributesForwardConf>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttributesForwardConf {
    /// Configuration to insert custom attributes/labels in metrics
    #[serde(rename = "static")]
    pub(crate) insert: Option<Vec<Insert>>,
    /// Configuration to forward headers or body values from the request to custom attributes/labels in metrics
    pub(crate) request: Option<Forward>,
    /// Configuration to forward headers or body values from the response to custom attributes/labels in metrics
    pub(crate) response: Option<Forward>,
    /// Configuration to forward values from the context to custom attributes/labels in metrics
    pub(crate) context: Option<Vec<ContextForward>>,
    /// Configuration to forward values from the error to custom attributes/labels in metrics
    pub(crate) errors: Option<ErrorsForward>,
}

#[derive(Clone, JsonSchema, Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
/// Configuration to insert custom attributes/labels in metrics
pub(crate) struct Insert {
    pub(crate) name: String,
    pub(crate) value: String,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct Forward {
    /// Forward header values as custom attributes/labels in metrics
    pub(crate) header: Option<Vec<HeaderForward>>,
    /// Forward body values as custom attributes/labels in metrics
    pub(crate) body: Option<Vec<BodyForward>>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ErrorsForward {
    /// Will include the error message in a "message" attribute
    #[serde(default)]
    pub(crate) include_messages: bool,
    /// Forward extensions values as custom attributes/labels in metrics
    pub(crate) extensions: Option<Vec<BodyForward>>,
}

#[derive(Clone, JsonSchema, Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[serde(untagged)]
/// Configuration to forward header values in metric labels
pub(crate) enum HeaderForward {
    /// Using a named header
    Named {
        #[schemars(schema_with = "string_schema")]
        #[serde(deserialize_with = "deserialize_header_name")]
        named: HeaderName,
        rename: Option<String>,
        default: Option<String>,
    },
    /// Using a regex on the header name
    Matching {
        #[schemars(schema_with = "string_schema")]
        #[serde(deserialize_with = "deserialize_regex")]
        matching: Regex,
    },
}

#[derive(Clone, JsonSchema, Deserialize, Debug)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
/// Configuration to forward body values in metric attributes/labels
pub(crate) struct BodyForward {
    #[schemars(schema_with = "string_schema")]
    #[serde(deserialize_with = "deserialize_json_query")]
    pub(crate) path: JSONQuery,
    pub(crate) name: String,
    pub(crate) default: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Configuration to forward context values in metric attributes/labels
pub(crate) struct ContextForward {
    pub(crate) named: String,
    pub(crate) rename: Option<String>,
    pub(crate) default: Option<String>,
}

impl HeaderForward {
    pub(crate) fn get_attributes_from_headers(
        &self,
        headers: &HeaderMap,
    ) -> HashMap<String, String> {
        let mut attributes = HashMap::new();
        match self {
            HeaderForward::Named {
                named,
                rename,
                default,
            } => {
                let value = headers.get(named);
                if let Some(value) = value
                    .and_then(|v| v.to_str().ok()?.to_string().into())
                    .or_else(|| default.clone())
                {
                    attributes.insert(rename.clone().unwrap_or_else(|| named.to_string()), value);
                }
            }
            HeaderForward::Matching { matching } => {
                headers
                    .iter()
                    .filter(|(name, _)| matching.is_match(name.as_str()))
                    .for_each(|(name, value)| {
                        if let Ok(value) = value.to_str() {
                            attributes.insert(name.to_string(), value.to_string());
                        }
                    });
            }
        }

        attributes
    }
}

impl Forward {
    pub(crate) fn merge(&mut self, to_merge: Self) {
        match (&mut self.body, to_merge.body) {
            (Some(body), Some(body_to_merge)) => {
                body.extend(body_to_merge);
            }
            (None, Some(body_to_merge)) => {
                self.body = Some(body_to_merge);
            }
            _ => {}
        }
        match (&mut self.header, to_merge.header) {
            (Some(header), Some(header_to_merge)) => {
                header.extend(header_to_merge);
            }
            (None, Some(header_to_merge)) => {
                self.header = Some(header_to_merge);
            }
            _ => {}
        }
    }
}

impl ErrorsForward {
    pub(crate) fn merge(&mut self, to_merge: Self) {
        match (&mut self.extensions, to_merge.extensions) {
            (Some(extensions), Some(extensions_to_merge)) => {
                extensions.extend(extensions_to_merge);
            }
            (None, Some(extensions_to_merge)) => {
                self.extensions = Some(extensions_to_merge);
            }
            _ => {}
        }
        self.include_messages = to_merge.include_messages;
    }

    pub(crate) fn get_attributes_from_error(&self, err: &BoxError) -> HashMap<String, String> {
        let mut attributes = HashMap::new();
        if let Some(fetch_error) = err
            .source()
            .and_then(|e| e.downcast_ref::<FetchError>())
            .or_else(|| err.downcast_ref::<FetchError>())
        {
            let gql_error = fetch_error.to_graphql_error(None);
            // Include error message
            if self.include_messages {
                attributes.insert("message".to_string(), gql_error.message);
            }
            // Extract data from extensions
            if let Some(extensions_fw) = &self.extensions {
                for ext_fw in extensions_fw {
                    let output = ext_fw.path.execute(&gql_error.extensions).unwrap();
                    if let Some(val) = output {
                        if let Value::String(val_str) = val {
                            attributes.insert(ext_fw.name.clone(), val_str);
                        } else {
                            attributes.insert(ext_fw.name.clone(), val.to_string());
                        }
                    } else if let Some(default_val) = &ext_fw.default {
                        attributes.insert(ext_fw.name.clone(), default_val.clone());
                    }
                }
            }
        } else if self.include_messages {
            attributes.insert("message".to_string(), err.to_string());
        }

        attributes
    }
}

impl AttributesForwardConf {
    pub(crate) fn get_attributes_from_router_response(
        &self,
        parts: &Parts,
        context: &Context,
        first_response: &Option<graphql::Response>,
    ) -> HashMap<String, String> {
        let mut attributes = HashMap::new();

        // Fill from static
        if let Some(to_insert) = &self.insert {
            for Insert { name, value } in to_insert {
                attributes.insert(name.clone(), value.clone());
            }
        }
        // Fill from context
        if let Some(from_context) = &self.context {
            for ContextForward {
                named,
                default,
                rename,
            } in from_context
            {
                match context.get::<_, String>(named) {
                    Ok(Some(value)) => {
                        attributes.insert(rename.as_ref().unwrap_or(named).clone(), value);
                    }
                    _ => {
                        if let Some(default_val) = default {
                            attributes.insert(
                                rename.as_ref().unwrap_or(named).clone(),
                                default_val.clone(),
                            );
                        }
                    }
                };
            }
        }

        // Fill from response
        if let Some(from_response) = &self.response {
            if let Some(header_forward) = &from_response.header {
                attributes.extend(header_forward.iter().fold(
                    HashMap::new(),
                    |mut acc, current| {
                        acc.extend(current.get_attributes_from_headers(&parts.headers));
                        acc
                    },
                ));
            }

            if let Some(body_forward) = &from_response.body {
                if let Some(body) = &first_response {
                    for body_fw in body_forward {
                        let output = body_fw.path.execute(body).unwrap();
                        if let Some(val) = output {
                            if let Value::String(val_str) = val {
                                attributes.insert(body_fw.name.clone(), val_str);
                            } else {
                                attributes.insert(body_fw.name.clone(), val.to_string());
                            }
                        } else if let Some(default_val) = &body_fw.default {
                            attributes.insert(body_fw.name.clone(), default_val.clone());
                        }
                    }
                }
            }
        }

        attributes
    }

    /// Get attributes from context
    pub(crate) fn get_attributes_from_context(&self, context: &Context) -> HashMap<String, String> {
        let mut attributes = HashMap::new();

        if let Some(from_context) = &self.context {
            for ContextForward {
                named,
                default,
                rename,
            } in from_context
            {
                match context.get::<_, String>(named) {
                    Ok(Some(value)) => {
                        attributes.insert(rename.as_ref().unwrap_or(named).clone(), value);
                    }
                    _ => {
                        if let Some(default_val) = default {
                            attributes.insert(
                                rename.as_ref().unwrap_or(named).clone(),
                                default_val.clone(),
                            );
                        }
                    }
                };
            }
        }

        attributes
    }

    pub(crate) fn get_attributes_from_response<T: Serialize>(
        &self,
        headers: &HeaderMap,
        body: &T,
    ) -> HashMap<String, String> {
        let mut attributes = HashMap::new();

        // Fill from static
        if let Some(to_insert) = &self.insert {
            for Insert { name, value } in to_insert {
                attributes.insert(name.clone(), value.clone());
            }
        }
        // Fill from response
        if let Some(from_response) = &self.response {
            if let Some(headers_forward) = &from_response.header {
                attributes.extend(headers_forward.iter().fold(
                    HashMap::new(),
                    |mut acc, current| {
                        acc.extend(current.get_attributes_from_headers(headers));
                        acc
                    },
                ));
            }
            if let Some(body_forward) = &from_response.body {
                for body_fw in body_forward {
                    let output = body_fw.path.execute(body).unwrap();
                    if let Some(val) = output {
                        if let Value::String(val_str) = val {
                            attributes.insert(body_fw.name.clone(), val_str);
                        } else {
                            attributes.insert(body_fw.name.clone(), val.to_string());
                        }
                    } else if let Some(default_val) = &body_fw.default {
                        attributes.insert(body_fw.name.clone(), default_val.clone());
                    }
                }
            }
        }

        attributes
    }

    pub(crate) fn get_attributes_from_request(
        &self,
        headers: &HeaderMap,
        body: &Request,
    ) -> HashMap<String, String> {
        let mut attributes = HashMap::new();

        // Fill from static
        if let Some(to_insert) = &self.insert {
            for Insert { name, value } in to_insert {
                attributes.insert(name.clone(), value.clone());
            }
        }
        // Fill from response
        if let Some(from_request) = &self.request {
            if let Some(headers_forward) = &from_request.header {
                attributes.extend(headers_forward.iter().fold(
                    HashMap::new(),
                    |mut acc, current| {
                        acc.extend(current.get_attributes_from_headers(headers));
                        acc
                    },
                ));
            }
            if let Some(body_forward) = &from_request.body {
                for body_fw in body_forward {
                    let output = body_fw.path.execute(body).ok().flatten();
                    if let Some(val) = output {
                        if let Value::String(val_str) = val {
                            attributes.insert(body_fw.name.clone(), val_str);
                        } else {
                            attributes.insert(body_fw.name.clone(), val.to_string());
                        }
                    } else if let Some(default_val) = &body_fw.default {
                        attributes.insert(body_fw.name.clone(), default_val.clone());
                    }
                }
            }
        }

        attributes
    }

    pub(crate) fn get_attributes_from_error(&self, err: &BoxError) -> HashMap<String, String> {
        self.errors
            .as_ref()
            .map(|e| e.get_attributes_from_error(err))
            .unwrap_or_default()
    }
}

fn string_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
    String::json_schema(gen)
}

#[derive(Default)]
pub(crate) struct MetricsBuilder {
    exporters: Vec<MetricsExporterHandle>,
    meter_providers: Vec<Arc<dyn MeterProvider + Send + Sync + 'static>>,
    custom_endpoints: MultiMap<ListenAddr, Endpoint>,
    apollo_metrics: Sender,
}

impl MetricsBuilder {
    pub(crate) fn exporters(&mut self) -> Vec<MetricsExporterHandle> {
        std::mem::take(&mut self.exporters)
    }
    pub(crate) fn meter_provider(&mut self) -> AggregateMeterProvider {
        AggregateMeterProvider::new(std::mem::take(&mut self.meter_providers))
    }
    pub(crate) fn custom_endpoints(&mut self) -> MultiMap<ListenAddr, Endpoint> {
        std::mem::take(&mut self.custom_endpoints)
    }

    pub(crate) fn apollo_metrics_provider(&mut self) -> Sender {
        self.apollo_metrics.clone()
    }
}

impl MetricsBuilder {
    fn with_exporter<T: Send + Sync + 'static>(mut self, handle: T) -> Self {
        self.exporters.push(Box::new(handle));
        self
    }

    fn with_meter_provider<T: MeterProvider + Send + Sync + 'static>(
        mut self,
        meter_provider: T,
    ) -> Self {
        self.meter_providers.push(Arc::new(meter_provider));
        self
    }

    fn with_custom_endpoint(mut self, listen_addr: ListenAddr, endpoint: Endpoint) -> Self {
        self.custom_endpoints.insert(listen_addr, endpoint);
        self
    }

    fn with_apollo_metrics_collector(mut self, apollo_metrics: Sender) -> Self {
        self.apollo_metrics = apollo_metrics;
        self
    }
}

pub(crate) trait MetricsConfigurator {
    fn apply(
        &self,
        builder: MetricsBuilder,
        metrics_config: &MetricsCommon,
    ) -> Result<MetricsBuilder, BoxError>;
}

#[derive(Clone)]
pub(crate) struct BasicMetrics {
    pub(crate) http_requests_total: AggregateCounter<u64>,
    pub(crate) http_requests_error_total: AggregateCounter<u64>,
    pub(crate) http_requests_duration: AggregateValueRecorder<f64>,
}

impl BasicMetrics {
    pub(crate) fn new(meter_provider: &AggregateMeterProvider) -> BasicMetrics {
        let meter = meter_provider.meter("apollo/router", None);
        BasicMetrics {
            http_requests_total: meter.build_counter(|m| {
                m.u64_counter("apollo_router_http_requests_total")
                    .with_description("Total number of HTTP requests made.")
                    .init()
            }),
            http_requests_error_total: meter.build_counter(|m| {
                m.u64_counter("apollo_router_http_requests_error_total")
                    .with_description("Total number of HTTP requests in error made.")
                    .init()
            }),
            http_requests_duration: meter.build_value_recorder(|m| {
                m.f64_value_recorder("apollo_router_http_request_duration_seconds")
                    .with_description("Total number of HTTP requests made.")
                    .init()
            }),
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct AggregateMeterProvider(Vec<Arc<dyn MeterProvider + Send + Sync + 'static>>);
impl AggregateMeterProvider {
    pub(crate) fn new(
        meters: Vec<Arc<dyn MeterProvider + Send + Sync + 'static>>,
    ) -> AggregateMeterProvider {
        AggregateMeterProvider(meters)
    }

    pub(crate) fn meter(
        &self,
        instrumentation_name: &'static str,
        instrumentation_version: Option<&'static str>,
    ) -> AggregateMeter {
        AggregateMeter(
            self.0
                .iter()
                .map(|p| Arc::new(p.meter(instrumentation_name, instrumentation_version)))
                .collect(),
        )
    }
}

#[derive(Clone)]
pub(crate) struct AggregateMeter(Vec<Arc<Meter>>);
impl AggregateMeter {
    pub(crate) fn build_counter<T: Into<Number> + Copy>(
        &self,
        build: fn(&Meter) -> Counter<T>,
    ) -> AggregateCounter<T> {
        AggregateCounter(self.0.iter().map(|m| build(m)).collect())
    }

    pub(crate) fn build_value_recorder<T: Into<Number> + Copy>(
        &self,
        build: fn(&Meter) -> ValueRecorder<T>,
    ) -> AggregateValueRecorder<T> {
        AggregateValueRecorder(self.0.iter().map(|m| build(m)).collect())
    }
}

#[derive(Clone)]
pub(crate) struct AggregateCounter<T: Into<Number> + Copy>(Vec<Counter<T>>);
impl<T> AggregateCounter<T>
where
    T: Into<Number> + Copy,
{
    pub(crate) fn add(&self, value: T, attributes: &[KeyValue]) {
        for counter in &self.0 {
            counter.add(value, attributes)
        }
    }
}

#[derive(Clone)]
pub(crate) struct AggregateValueRecorder<T: Into<Number> + Copy>(Vec<ValueRecorder<T>>);
impl<T> AggregateValueRecorder<T>
where
    T: Into<Number> + Copy,
{
    pub(crate) fn record(&self, value: T, attributes: &[KeyValue]) {
        for value_recorder in &self.0 {
            value_recorder.record(value, attributes)
        }
    }
}
