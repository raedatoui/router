use std::collections::HashMap;
use std::sync::Arc;

use futures::future::join_all;
use futures::prelude::*;
use opentelemetry::trace::SpanKind;
use tokio::sync::broadcast::Sender;
use tokio_stream::wrappers::BroadcastStream;
use tracing::Instrument;

use super::log;
use super::DeferredNode;
use super::PlanNode;
use super::QueryPlan;
use super::QueryPlanOptions;
use crate::error::Error;
use crate::graphql::Request;
use crate::graphql::Response;
use crate::json_ext::Path;
use crate::json_ext::Value;
use crate::json_ext::ValueExt;
use crate::query_planner::FlattenNode;
use crate::query_planner::Primary;
use crate::query_planner::FETCH_SPAN_NAME;
use crate::query_planner::FLATTEN_SPAN_NAME;
use crate::query_planner::PARALLEL_SPAN_NAME;
use crate::query_planner::SEQUENCE_SPAN_NAME;
use crate::services::subgraph_service::SubgraphServiceFactory;
use crate::*;

impl QueryPlan {
    /// Execute the plan and return a [`Response`].
    pub(crate) async fn execute<'a, SF>(
        &self,
        context: &'a Context,
        service_factory: &'a Arc<SF>,
        supergraph_request: &'a Arc<http::Request<Request>>,
        schema: &'a Schema,
        sender: futures::channel::mpsc::Sender<Response>,
    ) -> Response
    where
        SF: SubgraphServiceFactory,
    {
        let root = Path::empty();

        log::trace_query_plan(&self.root);
        let deferred_fetches = HashMap::new();
        let (value, subselection, errors) = self
            .root
            .execute_recursively(
                &ExecutionParameters {
                    context,
                    service_factory,
                    schema,
                    supergraph_request,
                    deferred_fetches: &deferred_fetches,
                    query: &self.query,
                    options: &self.options,
                },
                &root,
                &Value::default(),
                sender,
            )
            .await;

        Response::builder()
            .data(value)
            .and_subselection(subselection)
            .errors(errors)
            .build()
    }

    pub fn contains_mutations(&self) -> bool {
        self.root.contains_mutations()
    }
}

// holds the query plan executon arguments that do not change between calls
pub(crate) struct ExecutionParameters<'a, SF> {
    pub(crate) context: &'a Context,
    pub(crate) service_factory: &'a Arc<SF>,
    pub(crate) schema: &'a Schema,
    pub(crate) supergraph_request: &'a Arc<http::Request<Request>>,
    pub(crate) deferred_fetches: &'a HashMap<String, Sender<(Value, Vec<Error>)>>,
    pub(crate) query: &'a Arc<Query>,
    pub(crate) options: &'a QueryPlanOptions,
}

impl PlanNode {
    fn execute_recursively<'a, SF>(
        &'a self,
        parameters: &'a ExecutionParameters<'a, SF>,
        current_dir: &'a Path,
        parent_value: &'a Value,
        sender: futures::channel::mpsc::Sender<Response>,
    ) -> future::BoxFuture<(Value, Option<String>, Vec<Error>)>
    where
        SF: SubgraphServiceFactory,
    {
        Box::pin(async move {
            tracing::trace!("executing plan:\n{:#?}", self);
            let mut value;
            let mut errors;
            let mut subselection = None;

            match self {
                PlanNode::Sequence { nodes } => {
                    value = parent_value.clone();
                    errors = Vec::new();
                    let span = tracing::info_span!(SEQUENCE_SPAN_NAME);
                    for node in nodes {
                        let (v, subselect, err) = node
                            .execute_recursively(parameters, current_dir, &value, sender.clone())
                            .instrument(span.clone())
                            .in_current_span()
                            .await;
                        value.deep_merge(v);
                        errors.extend(err.into_iter());
                        subselection = subselect;
                    }
                }
                PlanNode::Parallel { nodes } => {
                    value = Value::default();
                    errors = Vec::new();

                    let span = tracing::info_span!(PARALLEL_SPAN_NAME);
                    let mut stream: stream::FuturesUnordered<_> = nodes
                        .iter()
                        .map(|plan| {
                            plan.execute_recursively(
                                parameters,
                                current_dir,
                                parent_value,
                                sender.clone(),
                            )
                            .instrument(span.clone())
                        })
                        .collect();

                    while let Some((v, _subselect, err)) = stream
                        .next()
                        .instrument(span.clone())
                        .in_current_span()
                        .await
                    {
                        value.deep_merge(v);
                        errors.extend(err.into_iter());
                    }
                }
                PlanNode::Flatten(FlattenNode { path, node }) => {
                    // Note that the span must be `info` as we need to pick this up in apollo tracing
                    let current_dir = current_dir.join(path);
                    let (v, subselect, err) = node
                        .execute_recursively(
                            parameters,
                            // this is the only command that actually changes the "current dir"
                            &current_dir,
                            parent_value,
                            sender,
                        )
                        .instrument(
                            tracing::info_span!(FLATTEN_SPAN_NAME, apollo_private.path = %current_dir),
                        )
                        .await;

                    value = v;
                    errors = err;
                    subselection = subselect;
                }
                PlanNode::Fetch(fetch_node) => {
                    let fetch_time_offset =
                        parameters.context.created_at.elapsed().as_nanos() as i64;
                    match fetch_node
                        .fetch_node(parameters, parent_value, current_dir)
                        .instrument(tracing::info_span!(
                            FETCH_SPAN_NAME,
                            "otel.kind" = %SpanKind::Internal,
                            "apollo.subgraph.name" = fetch_node.service_name.as_str(),
                            "apollo_private.sent_time_offset" = fetch_time_offset
                        ))
                        .await
                    {
                        Ok((v, e)) => {
                            value = v;
                            errors = e;
                        }
                        Err(err) => {
                            failfast_error!("Fetch error: {}", err);
                            errors = vec![err.to_graphql_error(Some(current_dir.to_owned()))];
                            value = Value::default();
                        }
                    }
                }
                PlanNode::Defer {
                    primary:
                        Primary {
                            path: _primary_path,
                            subselection: primary_subselection,
                            node,
                        },
                    deferred,
                } => {
                    let mut deferred_fetches: HashMap<String, Sender<(Value, Vec<Error>)>> =
                        HashMap::new();
                    let mut futures = Vec::new();

                    let (primary_sender, _) = tokio::sync::broadcast::channel::<Value>(1);

                    for deferred_node in deferred {
                        let fut = deferred_node.execute(
                            parameters,
                            parent_value,
                            sender.clone(),
                            &primary_sender,
                            &mut deferred_fetches,
                        );

                        futures.push(fut);
                    }

                    tokio::task::spawn(
                        async move {
                            join_all(futures).await;
                        }
                        .in_current_span(),
                    );

                    value = parent_value.clone();
                    errors = Vec::new();
                    let span = tracing::info_span!("primary");
                    if let Some(node) = node {
                        let (v, _subselect, err) = node
                            .execute_recursively(
                                &ExecutionParameters {
                                    context: parameters.context,
                                    service_factory: parameters.service_factory,
                                    schema: parameters.schema,
                                    supergraph_request: parameters.supergraph_request,
                                    deferred_fetches: &deferred_fetches,
                                    options: parameters.options,
                                    query: parameters.query,
                                },
                                current_dir,
                                &value,
                                sender,
                            )
                            .instrument(span.clone())
                            .in_current_span()
                            .await;
                        let _guard = span.enter();
                        value.deep_merge(v);
                        errors.extend(err.into_iter());
                        subselection = primary_subselection.clone();

                        let _ = primary_sender.send(value.clone());
                    } else {
                        let _guard = span.enter();

                        subselection = primary_subselection.clone();

                        let _ = primary_sender.send(value.clone());
                    }
                }
                PlanNode::Condition {
                    condition,
                    if_clause,
                    else_clause,
                } => {
                    value = Value::default();
                    errors = Vec::new();

                    let v = parameters
                        .query
                        .variable_value(
                            parameters
                                .supergraph_request
                                .body()
                                .operation_name
                                .as_deref(),
                            condition.as_str(),
                            &parameters.supergraph_request.body().variables,
                        )
                        .unwrap_or(&Value::Bool(true)); // the defer if clause is mandatory, and defaults to true

                    if let &Value::Bool(true) = v {
                        //FIXME: should we show an error if the if_node was not present?
                        if let Some(node) = if_clause {
                            let span = tracing::info_span!("condition_if");
                            let (v, subselect, err) = node
                                .execute_recursively(
                                    parameters,
                                    current_dir,
                                    parent_value,
                                    sender.clone(),
                                )
                                .instrument(span.clone())
                                .in_current_span()
                                .await;
                            value.deep_merge(v);
                            errors.extend(err.into_iter());
                            subselection = subselect;
                        }
                    } else if let Some(node) = else_clause {
                        let span = tracing::info_span!("condition_else");
                        let (v, subselect, err) = node
                            .execute_recursively(
                                parameters,
                                current_dir,
                                parent_value,
                                sender.clone(),
                            )
                            .instrument(span.clone())
                            .in_current_span()
                            .await;
                        value.deep_merge(v);
                        errors.extend(err.into_iter());
                        subselection = subselect;
                    }
                }
            }

            (value, subselection, errors)
        })
    }
}

impl DeferredNode {
    fn execute<'a, 'b, SF>(
        &'b self,
        parameters: &'a ExecutionParameters<'a, SF>,
        parent_value: &Value,
        sender: futures::channel::mpsc::Sender<Response>,
        primary_sender: &Sender<Value>,
        deferred_fetches: &mut HashMap<String, Sender<(Value, Vec<Error>)>>,
    ) -> impl Future<Output = ()>
    where
        SF: SubgraphServiceFactory,
    {
        let mut deferred_receivers = Vec::new();

        for d in self.depends.iter() {
            match deferred_fetches.get(&d.id) {
                None => {
                    let (sender, receiver) = tokio::sync::broadcast::channel(1);
                    deferred_fetches.insert(d.id.clone(), sender.clone());
                    deferred_receivers.push(BroadcastStream::new(receiver).into_future());
                }
                Some(sender) => {
                    let receiver = sender.subscribe();
                    deferred_receivers.push(BroadcastStream::new(receiver).into_future());
                }
            }
        }

        // if a deferred node has no depends (ie not waiting for data from fetches) then it has to
        // wait until the primary response is entirely created.
        //
        // If the depends list is not empty, the inner node can start working on the fetched data, then
        // it is merged into the primary response before applying the subselection
        let is_depends_empty = self.depends.is_empty();

        let mut stream: stream::FuturesUnordered<_> = deferred_receivers.into_iter().collect();
        //FIXME/ is there a solution without cloning the entire node? Maybe it could be moved instead?
        let deferred_inner = self.node.clone();
        let deferred_path = self.path.clone();
        let subselection = self.subselection();
        let label = self.label.clone();
        let mut tx = sender;
        let sc = parameters.schema.clone();
        let orig = parameters.supergraph_request.clone();
        let sf = parameters.service_factory.clone();
        let ctx = parameters.context.clone();
        let opt = parameters.options.clone();
        let query = parameters.query.clone();
        let mut primary_receiver = primary_sender.subscribe();
        let mut value = parent_value.clone();

        async move {
            let mut errors = Vec::new();

            if is_depends_empty {
                let primary_value = primary_receiver.recv().await.unwrap_or_default();
                value.deep_merge(primary_value);
            } else {
                while let Some((v, _remaining)) = stream.next().await {
                    // a Err(RecvError) means either that the fetch was not performed and the
                    // sender was dropped, possibly because there was no need to do it,
                    // or because it is lagging, but here we only send one message so it
                    // will not happen
                    if let Some(Ok((deferred_value, err))) = v {
                        value.deep_merge(deferred_value);
                        errors.extend(err.into_iter())
                    }
                }
            }

            let span = tracing::info_span!("deferred");
            let deferred_fetches = HashMap::new();

            if let Some(node) = deferred_inner {
                let (mut v, node_subselection, err) = node
                    .execute_recursively(
                        &ExecutionParameters {
                            context: &ctx,
                            service_factory: &sf,
                            schema: &sc,
                            supergraph_request: &orig,
                            deferred_fetches: &deferred_fetches,
                            query: &query,
                            options: &opt,
                        },
                        &Path::default(),
                        &value,
                        tx.clone(),
                    )
                    .instrument(span.clone())
                    .in_current_span()
                    .await;

                if !is_depends_empty {
                    let primary_value = primary_receiver.recv().await.unwrap_or_default();
                    v.deep_merge(primary_value);
                }

                if let Err(e) = tx
                    .send(
                        Response::builder()
                            .data(v)
                            .errors(err)
                            .and_path(Some(deferred_path.clone()))
                            .and_subselection(subselection.or(node_subselection))
                            .and_label(label)
                            .build(),
                    )
                    .await
                {
                    tracing::error!(
                        "error sending deferred response at path {}: {:?}",
                        deferred_path,
                        e
                    );
                };
                tx.disconnect();
            } else {
                let primary_value = primary_receiver.recv().await.unwrap_or_default();
                value.deep_merge(primary_value);

                if let Err(e) = tx
                    .send(
                        Response::builder()
                            .data(value)
                            .errors(errors)
                            .and_path(Some(deferred_path.clone()))
                            .and_subselection(subselection)
                            .and_label(label)
                            .build(),
                    )
                    .await
                {
                    tracing::error!(
                        "error sending deferred response at path {}: {:?}",
                        deferred_path,
                        e
                    );
                }
                tx.disconnect();
            };
        }
    }
}
