//! Plugin system for the router.
//!
//! Provides a customization mechanism for the router.
//!
//! Requests received by the router make their way through a processing pipeline. Each request is
//! processed at:
//!  - router
//!  - query planning
//!  - execution
//!  - subgraph (multiple in parallel if multiple subgraphs are accessed)
//!  stages.
//!
//! A plugin can choose to interact with the flow of requests at any or all of these stages of
//! processing. At each stage a [`Service`] is provided which provides an appropriate
//! mechanism for interacting with the request and response.

pub mod serde;
#[macro_use]
pub mod test;

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::Context;
use std::task::Poll;

use ::serde::de::DeserializeOwned;
use ::serde::Deserialize;
use async_trait::async_trait;
use futures::future::BoxFuture;
use multimap::MultiMap;
use once_cell::sync::Lazy;
use schemars::gen::SchemaGenerator;
use schemars::JsonSchema;
use tower::buffer::future::ResponseFuture;
use tower::buffer::Buffer;
use tower::BoxError;
use tower::Service;
use tower::ServiceBuilder;

use crate::layers::ServiceBuilderExt;
use crate::router_factory::Endpoint;
use crate::services::execution;
use crate::services::subgraph;
use crate::services::supergraph;
use crate::transport;
use crate::ListenAddr;

type InstanceFactory =
    fn(&serde_json::Value, Arc<String>) -> BoxFuture<Result<Box<dyn DynPlugin>, BoxError>>;

type SchemaFactory = fn(&mut SchemaGenerator) -> schemars::schema::Schema;

/// Initialise details for a plugin
#[non_exhaustive]
pub struct PluginInit<T> {
    /// Configuration
    pub config: T,
    /// Router Supergraph Schema (schema definition language)
    pub supergraph_sdl: Arc<String>,
}

impl<T> PluginInit<T>
where
    T: for<'de> Deserialize<'de>,
{
    /// Create a new PluginInit for the supplied config and SDL.
    pub fn new(config: T, supergraph_sdl: Arc<String>) -> Self {
        PluginInit {
            config,
            supergraph_sdl,
        }
    }

    /// Try to create a new PluginInit for the supplied JSON and SDL.
    ///
    /// This will fail if the supplied JSON cannot be deserialized into the configuration
    /// struct.
    pub fn try_new(
        config: serde_json::Value,
        supergraph_sdl: Arc<String>,
    ) -> Result<Self, BoxError> {
        let config: T = serde_json::from_value(config)?;
        Ok(PluginInit {
            config,
            supergraph_sdl,
        })
    }
}

/// Factories for plugin schema and configuration.
#[derive(Clone)]
pub(crate) struct PluginFactory {
    instance_factory: InstanceFactory,
    schema_factory: SchemaFactory,
    pub(crate) type_id: TypeId,
}

impl PluginFactory {
    pub(crate) async fn create_instance(
        &self,
        configuration: &serde_json::Value,
        supergraph_sdl: Arc<String>,
    ) -> Result<Box<dyn DynPlugin>, BoxError> {
        (self.instance_factory)(configuration, supergraph_sdl).await
    }

    #[cfg(test)]
    pub(crate) async fn create_instance_without_schema(
        &self,
        configuration: &serde_json::Value,
    ) -> Result<Box<dyn DynPlugin>, BoxError> {
        (self.instance_factory)(configuration, Default::default()).await
    }

    pub(crate) fn create_schema(&self, gen: &mut SchemaGenerator) -> schemars::schema::Schema {
        (self.schema_factory)(gen)
    }
}

static PLUGIN_REGISTRY: Lazy<Mutex<HashMap<String, PluginFactory>>> = Lazy::new(|| {
    let m = HashMap::new();
    Mutex::new(m)
});

/// Register a plugin factory.
pub fn register_plugin<P: Plugin>(name: String) {
    let plugin_factory = PluginFactory {
        instance_factory: |configuration, schema| {
            Box::pin(async move {
                let init = PluginInit::try_new(configuration.clone(), schema)?;
                let plugin = P::new(init).await?;
                Ok(Box::new(plugin) as Box<dyn DynPlugin>)
            })
        },
        schema_factory: |gen| gen.subschema_for::<<P as Plugin>::Config>(),
        type_id: TypeId::of::<P>(),
    };
    PLUGIN_REGISTRY
        .lock()
        .expect("Lock poisoned")
        .insert(name, plugin_factory);
}

/// Get a copy of the registered plugin factories.
pub(crate) fn plugins() -> HashMap<String, PluginFactory> {
    PLUGIN_REGISTRY.lock().expect("Lock poisoned").clone()
}

/// All router plugins must implement the Plugin trait.
///
/// This trait defines lifecycle hooks that enable hooking into Apollo Router services.
/// The trait also provides a default implementations for each hook, which returns the associated service unmodified.
/// For more information about the plugin lifecycle please check this documentation <https://www.apollographql.com/docs/router/customizations/native/#plugin-lifecycle>
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// The configuration for this plugin.
    /// Typically a `struct` with `#[derive(serde::Deserialize)]`.
    ///
    /// If a plugin is [registered][register_plugin()!],
    /// it can be enabled through the `plugins` section of Router YAML configuration
    /// by having a sub-section named after the plugin.
    /// The contents of this section are deserialized into this `Config` type
    /// and passed to [`Plugin::new`] as part of [`PluginInit`].
    type Config: JsonSchema + DeserializeOwned + Send;

    /// This is invoked once after the router starts and compiled-in
    /// plugins are registered.
    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError>
    where
        Self: Sized;

    /// This service runs at the very beginning and very end of the request lifecycle.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible.
    /// For example, this is a good opportunity to perform JWT verification before allowing a request to proceed further.
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        service
    }

    /// This service handles initiating the execution of a query plan after it's been generated.
    /// Define `execution_service` if your customization includes logic to govern execution (for example, if you want to block a particular query based on a policy decision).
    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        service
    }

    /// This service handles communication between the Apollo Router and your subgraphs.
    /// Define `subgraph_service` to configure this communication (for example, to dynamically add headers to pass to a subgraph).
    /// The `_subgraph_name` parameter is useful if you need to apply a customization only specific subgraphs.
    fn subgraph_service(
        &self,
        _subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService {
        service
    }

    /// Return the name of the plugin.
    fn name(&self) -> &'static str
    where
        Self: Sized,
    {
        get_type_of(self)
    }

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    ///
    /// This method is experimental and subject to change post 1.0
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        MultiMap::new()
    }

    /// Support downcasting.
    #[cfg(test)]
    fn as_any(&self) -> &dyn std::any::Any
    where
        Self: Sized,
    {
        self
    }
}

fn get_type_of<T>(_: &T) -> &'static str {
    std::any::type_name::<T>()
}

/// All router plugins must implement the DynPlugin trait.
///
/// This trait defines lifecycle hooks that enable hooking into Apollo Router services.
/// The trait also provides a default implementations for each hook, which returns the associated service unmodified.
/// For more information about the plugin lifecycle please check this documentation <https://www.apollographql.com/docs/router/customizations/native/#plugin-lifecycle>
#[async_trait]
pub(crate) trait DynPlugin: Send + Sync + 'static {
    /// This service runs at the very beginning and very end of the request lifecycle.
    /// It's the entrypoint of every requests and also the last hook before sending the response.
    /// Define supergraph_service if your customization needs to interact at the earliest or latest point possible.
    /// For example, this is a good opportunity to perform JWT verification before allowing a request to proceed further.
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService;

    /// This service handles initiating the execution of a query plan after it's been generated.
    /// Define `execution_service` if your customization includes logic to govern execution (for example, if you want to block a particular query based on a policy decision).
    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService;

    /// This service handles communication between the Apollo Router and your subgraphs.
    /// Define `subgraph_service` to configure this communication (for example, to dynamically add headers to pass to a subgraph).
    /// The `_subgraph_name` parameter is useful if you need to apply a customization only on specific subgraphs.
    fn subgraph_service(
        &self,
        _subgraph_name: &str,
        service: subgraph::BoxService,
    ) -> subgraph::BoxService;

    /// Return the name of the plugin.
    fn name(&self) -> &'static str;

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint>;

    fn as_any(&self) -> &dyn std::any::Any;
}

#[async_trait]
impl<T> DynPlugin for T
where
    T: Plugin,
    for<'de> <T as Plugin>::Config: Deserialize<'de>,
{
    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        self.supergraph_service(service)
    }

    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        self.execution_service(service)
    }

    fn subgraph_service(&self, name: &str, service: subgraph::BoxService) -> subgraph::BoxService {
        self.subgraph_service(name, service)
    }

    fn name(&self) -> &'static str {
        self.name()
    }

    /// Return one or several `Endpoint`s and `ListenAddr` and the router will serve your custom web Endpoint(s).
    fn web_endpoints(&self) -> MultiMap<ListenAddr, Endpoint> {
        self.web_endpoints()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Register a plugin with a group and a name
/// Grouping prevent name clashes for plugins, so choose something unique, like your domain name.
/// Plugins will appear in the configuration as a layer property called: {group}.{name}
#[macro_export]
macro_rules! register_plugin {
    ($group: literal, $name: literal, $plugin_type: ident) => {
        //  Artificial scope to avoid naming collisions
        const _: () = {
            #[$crate::_private::ctor::ctor]
            fn register_plugin() {
                let qualified_name = if $group == "" {
                    $name.to_string()
                } else {
                    format!("{}.{}", $group, $name)
                };

                $crate::plugin::register_plugin::<$plugin_type>(qualified_name);
            }
        };
    };
}

/// Handler represents a [`Plugin`] endpoint.
#[derive(Clone)]
pub(crate) struct Handler {
    service: Buffer<transport::BoxService, transport::Request>,
}

impl Handler {
    pub(crate) fn new(service: transport::BoxService) -> Self {
        Self {
            service: ServiceBuilder::new().buffered().service(service),
        }
    }
}

impl Service<transport::Request> for Handler {
    type Response = transport::Response;
    type Error = BoxError;
    type Future = ResponseFuture<BoxFuture<'static, Result<Self::Response, Self::Error>>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: transport::Request) -> Self::Future {
        self.service.call(req)
    }
}

impl From<transport::BoxService> for Handler {
    fn from(original: transport::BoxService) -> Self {
        Self::new(original)
    }
}
