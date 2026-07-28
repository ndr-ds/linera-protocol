// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    net::{IpAddr, SocketAddr},
    str::FromStr,
    sync::Arc,
    task::{Context, Poll},
};

use futures::{
    channel::mpsc, future::BoxFuture, stream::FuturesUnordered, FutureExt as _, StreamExt as _,
};
#[cfg(with_metrics)]
use linera_base::time::Instant;
use linera_base::{data_types::Blob, identifiers::ChainId, time::Duration};
use linera_core::{
    join_set_ext::JoinSet,
    node::NodeError,
    worker::{NetworkActions, Notification, Reason, WorkerState},
    JoinSetExt as _, ProcessConfirmedBlockMode, TaskHandle,
};
use linera_storage::{ChainShardAssignments, Storage};
use tokio::sync::{broadcast::error::RecvError, oneshot};
use tokio_util::sync::CancellationToken;
use tonic::{transport::Channel, Request, Response, Status};
use tower::{builder::ServiceBuilder, Layer, Service};
use tracing::{debug, error, info, instrument, trace, warn};

use super::{
    api::{
        self,
        notifier_service_client::NotifierServiceClient,
        validator_worker_client::ValidatorWorkerClient,
        validator_worker_server::{ValidatorWorker as ValidatorWorkerRpc, ValidatorWorkerServer},
        BlockProposal, ChainInfoQuery, ChainInfoResult, CrossChainRequest,
        HandlePendingBlobRequest, LiteCertificate, PendingBlobRequest, PendingBlobResult,
    },
    pool::GrpcConnectionPool,
    GrpcError, GRPC_MAX_MESSAGE_SIZE,
};
#[cfg(feature = "opentelemetry")]
use crate::propagation::get_traffic_type_from_request;
use crate::{
    config::{CrossChainConfig, NotificationConfig, ShardId, ValidatorInternalNetworkConfig},
    cross_chain_message_queue,
    routing::ShardRouter,
    HandleConfirmedCertificateRequest, HandleLiteCertRequest, HandleTimeoutCertificateRequest,
    HandleValidatedCertificateRequest,
};

/// Metadata key marking a request as already forwarded once by another worker.
/// Forwarded requests are never forwarded again, preventing routing loops while
/// worker routing tables are temporarily inconsistent during a migration.
const FORWARDED_METADATA_KEY: &str = "x-linera-forwarded";

type CrossChainSender = mpsc::Sender<(linera_core::data_types::CrossChainRequest, ShardId)>;
type NotificationSender = tokio::sync::broadcast::Sender<Notification>;

#[cfg(with_metrics)]
mod metrics {
    use std::sync::LazyLock;

    use linera_base::prometheus_util::{
        exponential_bucket_interval, linear_bucket_interval, register_histogram_vec,
        register_int_counter_vec,
    };
    use prometheus::{HistogramVec, IntCounterVec};

    use super::super::{ERROR_TYPE_LABEL, METHOD_NAME_LABEL, TRAFFIC_TYPE_LABEL};

    pub static SERVER_REQUEST_LATENCY: LazyLock<HistogramVec> = LazyLock::new(|| {
        register_histogram_vec(
            "server_request_latency",
            "Server request latency",
            &[METHOD_NAME_LABEL, TRAFFIC_TYPE_LABEL],
            linear_bucket_interval(1.0, 50.0, 5000.0),
        )
    });

    pub static SERVER_REQUEST_COUNT: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "server_request_count",
            "Server request count",
            &[METHOD_NAME_LABEL, TRAFFIC_TYPE_LABEL],
        )
    });

    pub static SERVER_REQUEST_SUCCESS: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "server_request_success",
            "Server request success",
            &[METHOD_NAME_LABEL, TRAFFIC_TYPE_LABEL],
        )
    });

    pub static SERVER_REQUEST_ERROR: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "server_request_error",
            "Server request error",
            &[METHOD_NAME_LABEL, TRAFFIC_TYPE_LABEL, ERROR_TYPE_LABEL],
        )
    });

    pub static SERVER_REQUEST_CANCELLED: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "server_request_cancelled",
            "Server requests whose handler future was dropped before completion (e.g. client-side timeout / disconnect)",
            &[METHOD_NAME_LABEL, TRAFFIC_TYPE_LABEL],
        )
    });

    pub static CROSS_CHAIN_MESSAGE_CHANNEL_FULL: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "cross_chain_message_channel_full",
            "Cross-chain message channel full",
            &[],
        )
    });

    pub static NOTIFICATIONS_SKIPPED_RECEIVER_LAG: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "notifications_skipped_receiver_lag",
            "Number of notifications skipped because receiver lagged behind sender",
            &[],
        )
    });

    pub static NOTIFICATIONS_DROPPED_NO_RECEIVER: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "notifications_dropped_no_receiver",
            "Number of notifications dropped because no receiver was available",
            &[],
        )
    });

    pub static NOTIFICATION_BATCH_SIZE: LazyLock<HistogramVec> = LazyLock::new(|| {
        register_histogram_vec(
            "notification_batch_size",
            "Number of notifications per batch sent to proxy",
            &[],
            exponential_bucket_interval(1.0, 250.0),
        )
    });

    pub static NOTIFICATION_BATCHES_SENT: LazyLock<IntCounterVec> = LazyLock::new(|| {
        register_int_counter_vec(
            "notification_batches_sent",
            "Total notification batches sent",
            &["status"],
        )
    });
}

/// Handles batched forwarding of notifications to proxy and exporters.
struct BatchForwarder {
    nickname: String,
    client: NotifierServiceClient<Channel>,
    exporter_clients: Vec<NotifierServiceClient<Channel>>,
    pending_notifications: Vec<Notification>,
    futures: FuturesUnordered<BoxFuture<'static, ()>>,
    batch_limit: usize,
    max_tasks: usize,
}

impl BatchForwarder {
    /// Spawns batch send tasks up to max_tasks limit.
    fn spawn_batches(&mut self) {
        while !self.pending_notifications.is_empty() && self.futures.len() < self.max_tasks {
            let chunk_size = std::cmp::min(self.batch_limit, self.pending_notifications.len());
            let batch: Vec<Notification> = self.pending_notifications.drain(..chunk_size).collect();

            #[cfg(with_metrics)]
            metrics::NOTIFICATION_BATCH_SIZE
                .with_label_values(&[])
                .observe(batch.len() as f64);

            let client = self.client.clone();
            let exporter_clients = self.exporter_clients.clone();
            let nickname = self.nickname.clone();

            self.futures.push(
                async move {
                    Self::send_batch(nickname, client, exporter_clients, batch).await;
                }
                .boxed(),
            );
        }
    }

    /// Returns true if there are no pending notifications and no in-flight tasks.
    fn is_fully_drained(&self) -> bool {
        self.pending_notifications.is_empty() && self.futures.is_empty()
    }

    /// Sends a batch of notifications to the proxy and exporters.
    async fn send_batch(
        nickname: String,
        mut client: NotifierServiceClient<Channel>,
        mut exporter_clients: Vec<NotifierServiceClient<Channel>>,
        batch: Vec<Notification>,
    ) {
        // Convert to proto notifications, logging any deserialization errors
        let mut proto_notifications = Vec::with_capacity(batch.len());
        for notification in &batch {
            match notification.clone().try_into() {
                Ok(proto) => proto_notifications.push(proto),
                Err(error) => {
                    warn!(
                        %error,
                        nickname,
                        ?notification.chain_id,
                        ?notification.reason,
                        "could not deserialize notification"
                    );
                }
            }
        }

        // Collect chain_ids for error logging
        let chain_ids: Vec<_> = batch.iter().map(|n| n.chain_id).collect();

        // Send batch to proxy
        let request = Request::new(api::NotificationBatch {
            notifications: proto_notifications.clone(),
        });
        let result = client.notify_batch(request).await;

        #[cfg(with_metrics)]
        {
            let status = if result.is_ok() { "success" } else { "error" };
            metrics::NOTIFICATION_BATCHES_SENT
                .with_label_values(&[status])
                .inc();
        }

        if let Err(error) = result {
            error!(
                %error,
                nickname,
                batch_size = proto_notifications.len(),
                ?chain_ids,
                "proxy: could not send notification batch",
            );
        }

        // Send NewBlock notifications to exporters
        let new_block_notifications: Vec<_> = batch
            .iter()
            .filter(|n| matches!(n.reason, Reason::NewBlock { .. }))
            .collect();

        let exporter_notifications: Vec<api::Notification> = new_block_notifications
            .iter()
            .filter_map(|n| (*n).clone().try_into().ok())
            .collect();

        if !exporter_notifications.is_empty() {
            let exporter_chain_ids: Vec<_> =
                new_block_notifications.iter().map(|n| n.chain_id).collect();

            for exporter_client in &mut exporter_clients {
                let request = Request::new(api::NotificationBatch {
                    notifications: exporter_notifications.clone(),
                });
                if let Err(error) = exporter_client.notify_batch(request).await {
                    error!(
                        %error,
                        nickname,
                        batch_size = exporter_notifications.len(),
                        ?exporter_chain_ids,
                        "block exporter: could not send notification batch",
                    );
                }
            }
        }
    }
}

/// A gRPC server exposing a validator's worker as a network service.
#[derive(Clone)]
pub struct GrpcServer<S>
where
    S: Storage,
{
    state: WorkerState<S>,
    shard_id: ShardId,
    network: ValidatorInternalNetworkConfig,
    cross_chain_sender: CrossChainSender,
    notification_sender: NotificationSender,
    /// Dynamic chain-to-shard routing table, shared with the cross-chain
    /// send task. Also acts as the per-chain handover barrier.
    router: Arc<ShardRouter>,
    /// Connection pool for forwarding misrouted requests to the worker that
    /// currently owns the target chain.
    forward_pool: GrpcConnectionPool,
}

/// The outcome of routing an incoming request for a chain.
enum Routed {
    /// The chain is owned by this worker; process the request locally while
    /// holding the routing read guard.
    Local(tokio::sync::OwnedRwLockReadGuard<ShardId>),
    /// The chain is owned by another worker; forward the request there.
    Forward(ValidatorWorkerClient<Channel>),
}

/// A handle to a running [`GrpcServer`] task.
pub struct GrpcServerHandle {
    handle: TaskHandle<Result<(), GrpcError>>,
}

impl GrpcServerHandle {
    /// Waits for the server task to complete.
    pub async fn join(self) -> Result<(), GrpcError> {
        self.handle.await?
    }
}

#[cfg(with_metrics)]
struct ServerRequestCancellationGuard {
    method_name: String,
    traffic_type: &'static str,
    completed: bool,
}

#[cfg(with_metrics)]
impl Drop for ServerRequestCancellationGuard {
    fn drop(&mut self) {
        if !self.completed {
            metrics::SERVER_REQUEST_CANCELLED
                .with_label_values(&[&self.method_name, self.traffic_type])
                .inc();
        }
    }
}

/// A Tower layer that records Prometheus metrics for gRPC requests.
#[derive(Clone)]
pub struct GrpcPrometheusMetricsMiddlewareLayer;

/// The Tower service produced by [`GrpcPrometheusMetricsMiddlewareLayer`].
#[derive(Clone)]
pub struct GrpcPrometheusMetricsMiddlewareService<T> {
    service: T,
}

impl<S> Layer<S> for GrpcPrometheusMetricsMiddlewareLayer {
    type Service = GrpcPrometheusMetricsMiddlewareService<S>;

    fn layer(&self, service: S) -> Self::Service {
        GrpcPrometheusMetricsMiddlewareService { service }
    }
}

impl<S, B> Service<http::Request<B>> for GrpcPrometheusMetricsMiddlewareService<S>
where
    S::Future: Send + 'static,
    S: Service<http::Request<B>> + std::marker::Send,
    B: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<S::Response, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        #[cfg(with_metrics)]
        let start = Instant::now();

        #[cfg(with_metrics)]
        let method_name = super::extract_grpc_method_name(request.uri().path()).to_owned();

        // Extract traffic type from request extensions (set by OtelContextLayer).
        // When opentelemetry is enabled but no baggage is set, defaults to "organic".
        // When opentelemetry is disabled, defaults to "unknown".
        #[cfg(all(with_metrics, feature = "opentelemetry"))]
        let traffic_type: &'static str = get_traffic_type_from_request(&request);
        #[cfg(all(with_metrics, not(feature = "opentelemetry")))]
        let traffic_type: &'static str = "unknown";

        let future = self.service.call(request);
        async move {
            #[cfg(with_metrics)]
            let mut cancellation_guard = ServerRequestCancellationGuard {
                method_name,
                traffic_type,
                completed: false,
            };
            let response = future.await?;
            #[cfg(with_metrics)]
            {
                cancellation_guard.completed = true;
                metrics::SERVER_REQUEST_LATENCY
                    .with_label_values(&[&cancellation_guard.method_name, traffic_type])
                    .observe(start.elapsed().as_secs_f64() * 1000.0);
                metrics::SERVER_REQUEST_COUNT
                    .with_label_values(&[&cancellation_guard.method_name, traffic_type])
                    .inc();
            }
            Ok(response)
        }
        .boxed()
    }
}

impl<S> GrpcServer<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    /// Spawns the gRPC server on the given host and port, returning a handle to the task.
    #[expect(clippy::too_many_arguments)]
    pub fn spawn(
        host: String,
        port: u16,
        state: WorkerState<S>,
        shard_id: ShardId,
        internal_network: ValidatorInternalNetworkConfig,
        cross_chain_config: &CrossChainConfig,
        notification_config: &NotificationConfig,
        shutdown_signal: CancellationToken,
        join_set: &mut JoinSet,
    ) -> GrpcServerHandle {
        info!(
            "spawning gRPC server on {}:{} for shard {}",
            host, port, shard_id
        );

        let (cross_chain_sender, cross_chain_receiver) =
            mpsc::channel(cross_chain_config.queue_size);

        let router = Arc::new(ShardRouter::new(
            internal_network.public_key,
            internal_network.shards.len(),
        ));

        // Give the worker a shard-routing sender for cross-chain requests generated
        // outside the normal `NetworkActions` return path (specifically, the
        // `RevertConfirm`s emitted after resetting a corrupted chain). The shard id
        // recorded here is only a hint: the send task re-resolves the target shard
        // through the router at send time.
        let state = {
            let routing_network = internal_network.clone();
            let routing_sender = cross_chain_sender.clone();
            state.with_outbound_cross_chain_sender(std::sync::Arc::new(move |request| {
                let shard_id = routing_network.get_shard_id(request.target_chain_id());
                if let Err(error) = routing_sender.clone().try_send((request, shard_id)) {
                    error!(%error, "dropping cross-chain request");
                }
            }))
        };

        let (notification_sender, _) =
            tokio::sync::broadcast::channel(notification_config.notification_queue_size);

        join_set.spawn_task({
            info!(
                nickname = state.nickname(),
                "spawning cross-chain queries thread on {} for shard {}", host, shard_id
            );
            Self::forward_cross_chain_queries(
                state.nickname().to_string(),
                internal_network.clone(),
                router.clone(),
                cross_chain_config.max_retries,
                Duration::from_millis(cross_chain_config.retry_delay_ms),
                Duration::from_millis(cross_chain_config.max_backoff_ms),
                Duration::from_millis(cross_chain_config.sender_delay_ms),
                cross_chain_config.sender_failure_rate,
                shard_id,
                cross_chain_receiver,
            )
        });

        let mut exporter_forwarded = false;
        for proxy in &internal_network.proxies {
            let receiver = notification_sender.subscribe();
            join_set.spawn_task({
                info!(
                    nickname = state.nickname(),
                    "spawning notifications thread on {} for shard {}", host, shard_id
                );
                let exporter_addresses = if exporter_forwarded {
                    vec![]
                } else {
                    exporter_forwarded = true;
                    internal_network.exporter_addresses()
                };
                Self::forward_notifications(
                    state.nickname().to_string(),
                    proxy.internal_address(&internal_network.protocol),
                    exporter_addresses,
                    receiver,
                    notification_config.clone(),
                )
            });
        }

        let (health_reporter, health_service) = tonic_health::server::health_reporter();

        let grpc_server = GrpcServer {
            state,
            shard_id,
            network: internal_network,
            cross_chain_sender,
            notification_sender,
            router,
            forward_pool: GrpcConnectionPool::default(),
        };

        let seeding_server = grpc_server.clone();

        let worker_node = ValidatorWorkerServer::new(grpc_server)
            .max_encoding_message_size(GRPC_MAX_MESSAGE_SIZE)
            .max_decoding_message_size(GRPC_MAX_MESSAGE_SIZE);

        let handle = join_set.spawn_task(async move {
            // Load the persisted chain-to-shard assignments before accepting any
            // request, so that migrated chains survive a worker restart.
            seeding_server.seed_router_from_storage().await;

            let server_address = SocketAddr::from((IpAddr::from_str(&host)?, port));

            let reflection_service = tonic_reflection::server::Builder::configure()
                .register_encoded_file_descriptor_set(crate::FILE_DESCRIPTOR_SET)
                .build_v1()?;

            health_reporter
                .set_serving::<ValidatorWorkerServer<Self>>()
                .await;

            #[cfg(feature = "opentelemetry")]
            let mut server = tonic::transport::Server::builder().layer(
                ServiceBuilder::new()
                    .layer(crate::propagation::OtelContextLayer)
                    .layer(GrpcPrometheusMetricsMiddlewareLayer)
                    .into_inner(),
            );
            #[cfg(not(feature = "opentelemetry"))]
            let mut server = tonic::transport::Server::builder().layer(
                ServiceBuilder::new()
                    .layer(GrpcPrometheusMetricsMiddlewareLayer)
                    .into_inner(),
            );
            server
                .add_service(health_service)
                .add_service(reflection_service)
                .add_service(worker_node)
                .serve_with_shutdown(server_address, shutdown_signal.cancelled_owned())
                .await?;

            Ok(())
        });

        GrpcServerHandle { handle }
    }

    /// Continuously waits for receiver to receive notifications and sends them to
    /// the proxy in batches for improved throughput.
    #[instrument(skip(receiver, config))]
    async fn forward_notifications(
        nickname: String,
        proxy_address: String,
        exporter_addresses: Vec<String>,
        mut receiver: tokio::sync::broadcast::Receiver<Notification>,
        config: NotificationConfig,
    ) {
        let channel = tonic::transport::Channel::from_shared(proxy_address.clone())
            .expect("Proxy URI should be valid")
            .connect_lazy();
        let client = NotifierServiceClient::new(channel)
            .max_encoding_message_size(GRPC_MAX_MESSAGE_SIZE)
            .max_decoding_message_size(GRPC_MAX_MESSAGE_SIZE);

        let exporter_clients: Vec<NotifierServiceClient<Channel>> = exporter_addresses
            .iter()
            .map(|address| {
                let channel = tonic::transport::Channel::from_shared(address.clone())
                    .expect("Exporter URI should be valid")
                    .connect_lazy();
                NotifierServiceClient::new(channel)
                    .max_encoding_message_size(GRPC_MAX_MESSAGE_SIZE)
                    .max_decoding_message_size(GRPC_MAX_MESSAGE_SIZE)
            })
            .collect::<Vec<_>>();

        let mut forwarder = BatchForwarder {
            nickname: nickname.clone(),
            client,
            exporter_clients,
            pending_notifications: Vec::new(),
            futures: FuturesUnordered::new(),
            batch_limit: config.notification_batch_size,
            max_tasks: config.notification_max_in_flight,
        };

        loop {
            tokio::select! {
                biased;

                result = receiver.recv() => {
                    match result {
                        Ok(notification) => {
                            forwarder.pending_notifications.push(notification);

                            if forwarder.futures.is_empty()
                               || (forwarder.pending_notifications.len() >= forwarder.batch_limit
                                   && forwarder.futures.len() < forwarder.max_tasks) {
                                forwarder.spawn_batches();
                            }
                        }
                        Err(RecvError::Lagged(skipped_count)) => {
                            warn!(
                                nickname,
                                skipped_count, "notification receiver lagged, messages were skipped"
                            );
                            #[cfg(with_metrics)]
                            metrics::NOTIFICATIONS_SKIPPED_RECEIVER_LAG
                                .with_label_values(&[])
                                .inc_by(skipped_count);
                        }
                        Err(RecvError::Closed) => {
                            warn!(
                                nickname,
                                "notification channel closed, draining pending notifications"
                            );
                            // Drain all pending notifications before exiting
                            loop {
                                forwarder.spawn_batches();
                                if forwarder.is_fully_drained() {
                                    break;
                                }
                                forwarder.futures.next().await;
                            }
                            break;
                        }
                    }
                }

                Some(()) = forwarder.futures.next() => {
                    forwarder.spawn_batches();
                }
            }
        }
    }

    fn handle_network_actions(&self, actions: NetworkActions) {
        let mut cross_chain_sender = self.cross_chain_sender.clone();
        let notification_sender = self.notification_sender.clone();

        for request in actions.cross_chain_requests {
            let shard_id = self.network.get_shard_id(request.target_chain_id());
            trace!(
                source_shard_id = self.shard_id,
                target_shard_id = shard_id,
                "Scheduling cross-chain query",
            );

            if let Err(error) = cross_chain_sender.try_send((request, shard_id)) {
                error!(%error, "dropping cross-chain request");
                #[cfg(with_metrics)]
                if error.is_full() {
                    metrics::CROSS_CHAIN_MESSAGE_CHANNEL_FULL
                        .with_label_values(&[])
                        .inc();
                }
            }
        }

        for notification in actions.notifications {
            trace!("Scheduling notification query");
            if let Err(error) = notification_sender.send(notification) {
                error!(%error, "dropping notification");
                #[cfg(with_metrics)]
                metrics::NOTIFICATIONS_DROPPED_NO_RECEIVER
                    .with_label_values(&[])
                    .inc();
            }
        }
    }

    #[instrument(skip_all, fields(nickname, %this_shard))]
    #[expect(clippy::too_many_arguments)]
    async fn forward_cross_chain_queries(
        nickname: String,
        network: ValidatorInternalNetworkConfig,
        router: Arc<ShardRouter>,
        cross_chain_max_retries: u32,
        cross_chain_retry_delay: Duration,
        cross_chain_max_backoff: Duration,
        cross_chain_sender_delay: Duration,
        cross_chain_sender_failure_rate: f32,
        this_shard: ShardId,
        receiver: mpsc::Receiver<(linera_core::data_types::CrossChainRequest, ShardId)>,
    ) {
        let pool = GrpcConnectionPool::default();
        // The shard id recorded at enqueue time is only a hint; the target shard
        // is re-resolved through the router on every (re-)send so that requests
        // queued across a chain migration still reach the chain's new worker.
        let handle_request =
            move |_shard_hint: ShardId, request: linera_core::data_types::CrossChainRequest| {
                let network = network.clone();
                let router = router.clone();
                let pool = pool.clone();
                async move {
                    let shard_id = router.shard_for(request.target_chain_id()).await;
                    let channel = pool.channel(network.shard(shard_id).http_address())?;
                    let mut client = ValidatorWorkerClient::new(channel)
                        .max_encoding_message_size(GRPC_MAX_MESSAGE_SIZE)
                        .max_decoding_message_size(GRPC_MAX_MESSAGE_SIZE);
                    client
                        .handle_cross_chain_request(Request::new(request.try_into()?))
                        .await?;
                    anyhow::Result::<_, anyhow::Error>::Ok(())
                }
            };
        cross_chain_message_queue::forward_cross_chain_queries(
            nickname,
            cross_chain_max_retries,
            cross_chain_retry_delay,
            cross_chain_max_backoff,
            cross_chain_sender_delay,
            cross_chain_sender_failure_rate,
            this_shard,
            receiver,
            handle_request,
        )
        .await;
    }

    fn log_request_success(method_name: &str, traffic_type: &str) {
        #![cfg_attr(not(with_metrics), allow(unused_variables))]
        #[cfg(with_metrics)]
        metrics::SERVER_REQUEST_SUCCESS
            .with_label_values(&[method_name, traffic_type])
            .inc();
    }

    fn log_request_error(method_name: &str, traffic_type: &str, error_type: &str) {
        #![cfg_attr(not(with_metrics), allow(unused_variables))]
        #[cfg(with_metrics)]
        metrics::SERVER_REQUEST_ERROR
            .with_label_values(&[method_name, traffic_type, error_type])
            .inc();
    }

    /// Extracts traffic type from a tonic request's extensions.
    #[cfg(feature = "opentelemetry")]
    fn get_traffic_type<R>(request: &Request<R>) -> &'static str {
        get_traffic_type_from_request(request)
    }

    /// Returns "unknown" when opentelemetry feature is disabled.
    #[cfg(not(feature = "opentelemetry"))]
    fn get_traffic_type<R>(_request: &Request<R>) -> &'static str {
        "unknown"
    }

    fn log_error(&self, error: &linera_core::worker::WorkerError, context: &str) {
        let nickname = self.state.nickname();
        if error.is_local() {
            error!(nickname, %error, "{}", context);
        } else {
            debug!(nickname, %error, "{}", context);
        }
    }

    /// Returns whether the request was already forwarded once by another worker.
    fn is_forwarded<R>(request: &Request<R>) -> bool {
        request.metadata().contains_key(FORWARDED_METADATA_KEY)
    }

    /// Wraps a message in a request marked as forwarded, so that the receiving
    /// worker will not forward it again.
    fn forwarding_request<R>(inner: R) -> Request<R> {
        let mut request = Request::new(inner);
        request.metadata_mut().insert(
            FORWARDED_METADATA_KEY,
            tonic::metadata::MetadataValue::from(1),
        );
        request
    }

    /// Returns a client for the worker currently serving the given shard.
    fn worker_client_for_shard(
        &self,
        shard_id: ShardId,
    ) -> Result<ValidatorWorkerClient<Channel>, Status> {
        let address = self.network.shard(shard_id).http_address();
        let channel = self
            .forward_pool
            .channel(address)
            .map_err(|_| Status::internal("could not connect to shard"))?;
        Ok(ValidatorWorkerClient::new(channel)
            .max_encoding_message_size(GRPC_MAX_MESSAGE_SIZE)
            .max_decoding_message_size(GRPC_MAX_MESSAGE_SIZE))
    }

    /// Routes an incoming request for `chain_id`.
    ///
    /// If this worker currently owns the chain, returns a read guard on the
    /// chain's routing entry; the caller must hold it for the duration of
    /// request processing so that a concurrent migration waits for the request
    /// to complete. Otherwise returns a client connected to the owning worker,
    /// unless the request was already forwarded once, in which case an error is
    /// returned to the (worker) caller, which will retry with fresh routing.
    async fn route(&self, chain_id: ChainId, forwarded: bool) -> Result<Routed, Status> {
        let guard = self.router.read_guard(chain_id).await;
        let target = *guard;
        if target == self.shard_id {
            return Ok(Routed::Local(guard));
        }
        drop(guard);
        if forwarded {
            return Err(Status::failed_precondition(format!(
                "chain {chain_id} is not assigned to shard {} (currently routed to shard \
                 {target}); refusing to forward an already-forwarded request",
                self.shard_id
            )));
        }
        trace!(
            %chain_id,
            from_shard = self.shard_id,
            to_shard = target,
            "forwarding misrouted request"
        );
        Ok(Routed::Forward(self.worker_client_for_shard(target)?))
    }

    /// Loads the persisted chain-to-shard assignments into the router.
    async fn seed_router_from_storage(&self) {
        match self.state.storage_client().read_shard_assignments().await {
            Ok(Some(assignments)) => {
                let version = assignments.version;
                let num_overrides = assignments.overrides.len();
                let mut overrides = Vec::with_capacity(num_overrides);
                for (chain_id, shard_id) in assignments.overrides {
                    let Ok(shard_id) = ShardId::try_from(shard_id) else {
                        warn!(%chain_id, shard_id, "ignoring invalid persisted shard assignment");
                        continue;
                    };
                    overrides.push((chain_id, shard_id));
                }
                self.router.seed(overrides).await;
                info!(
                    version,
                    num_overrides, "seeded shard router from persisted assignments"
                );
            }
            Ok(None) => {}
            Err(error) => {
                warn!(%error, "failed to read persisted shard assignments; using default routing");
            }
        }
    }

    /// Persists the router's current assignment overrides to shared storage.
    ///
    /// Note: migrations are assumed to be issued sequentially by a single
    /// administrator; concurrent writers would race on this read-modify-write.
    async fn persist_assignments(&self) -> Result<(), Status> {
        let storage = self.state.storage_client();
        let version = match storage.read_shard_assignments().await {
            Ok(assignments) => assignments.map_or(0, |assignments| assignments.version),
            Err(error) => {
                return Err(Status::internal(format!(
                    "failed to read shard assignments: {error}"
                )))
            }
        };
        let assignments = ChainShardAssignments {
            version: version + 1,
            overrides: self
                .router
                .overrides()
                .await
                .into_iter()
                .map(|(chain_id, shard_id)| (chain_id, shard_id as u64))
                .collect(),
        };
        storage
            .write_shard_assignments(&assignments)
            .await
            .map_err(|error| {
                Status::internal(format!("failed to write shard assignments: {error}"))
            })?;
        Ok(())
    }

    /// Parses and validates a `ShardAssignmentUpdate` message.
    fn parse_assignment(
        &self,
        update: api::ShardAssignmentUpdate,
    ) -> Result<(ChainId, ShardId), Status> {
        let chain_id: ChainId = update
            .chain_id
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?
            .try_into()?;
        let shard_id = usize::try_from(update.shard_id)
            .map_err(|_| Status::invalid_argument("invalid shard ID"))?;
        if shard_id >= self.network.shards.len() {
            return Err(Status::invalid_argument(format!(
                "shard {shard_id} does not exist; validator has {} shards",
                self.network.shards.len()
            )));
        }
        Ok((chain_id, shard_id))
    }
}

#[tonic::async_trait]
impl<S> ValidatorWorkerRpc for GrpcServer<S>
where
    S: Storage + Clone + Send + Sync + 'static,
{
    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_block_proposal(
        &self,
        request: Request<BlockProposal>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_block_proposal(Self::forwarding_request(inner))
                    .await;
            }
        };
        let proposal = inner.try_into()?;
        trace!(?proposal, "Handling block proposal");
        let (result, actions) = self.state.clone().handle_block_proposal(proposal).await;
        // Dispatch actions whether or not the proposal was accepted: a rejected
        // proposal can still advance the manager's `current_round` (via
        // `update_signed_proposal` on the `HasIncompatibleConfirmedVote` recovery
        // path), and subscribers need the resulting `NewRound` notification.
        self.handle_network_actions(actions);
        Ok(Response::new(match result {
            Ok(info) => {
                Self::log_request_success("handle_block_proposal", traffic_type);
                info.try_into()?
            }
            Err(error) => {
                Self::log_request_error("handle_block_proposal", traffic_type, &error.error_type());
                self.log_error(&error, "Failed to handle block proposal");
                NodeError::from(error).try_into()?
            }
        }))
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_lite_certificate(
        &self,
        request: Request<LiteCertificate>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_lite_certificate(Self::forwarding_request(inner))
                    .await;
            }
        };
        let HandleLiteCertRequest {
            certificate,
            wait_for_outgoing_messages,
        } = inner.try_into()?;
        trace!(?certificate, "Handling lite certificate");
        let (sender, receiver) = wait_for_outgoing_messages.then(oneshot::channel).unzip();
        match Box::pin(
            self.state
                .clone()
                .handle_lite_certificate(certificate, sender),
        )
        .await
        {
            Ok((info, actions)) => {
                Self::log_request_success("handle_lite_certificate", traffic_type);
                self.handle_network_actions(actions);
                if let Some(receiver) = receiver {
                    if let Err(e) = receiver.await {
                        error!("Failed to wait for message delivery: {e}");
                    }
                }
                Ok(Response::new(info.try_into()?))
            }
            Err(error) => {
                Self::log_request_error(
                    "handle_lite_certificate",
                    traffic_type,
                    &error.error_type(),
                );
                self.log_error(&error, "Failed to handle lite certificate");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_confirmed_certificate(
        &self,
        request: Request<api::HandleConfirmedCertificateRequest>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_confirmed_certificate(Self::forwarding_request(inner))
                    .await;
            }
        };
        let HandleConfirmedCertificateRequest {
            certificate,
            wait_for_outgoing_messages,
        } = inner.try_into()?;
        trace!(?certificate, "Handling certificate");
        let (sender, receiver) = wait_for_outgoing_messages.then(oneshot::channel).unzip();
        match self
            .state
            .clone()
            .handle_confirmed_certificate(certificate, ProcessConfirmedBlockMode::Auto, sender)
            .await
        {
            Ok((info, actions)) => {
                Self::log_request_success("handle_confirmed_certificate", traffic_type);
                self.handle_network_actions(actions);
                if let Some(receiver) = receiver {
                    if let Err(e) = receiver.await {
                        error!("Failed to wait for message delivery: {e}");
                    }
                }
                Ok(Response::new(info.try_into()?))
            }
            Err(error) => {
                Self::log_request_error(
                    "handle_confirmed_certificate",
                    traffic_type,
                    &error.error_type(),
                );
                self.log_error(&error, "Failed to handle confirmed certificate");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_validated_certificate(
        &self,
        request: Request<api::HandleValidatedCertificateRequest>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_validated_certificate(Self::forwarding_request(inner))
                    .await;
            }
        };
        let HandleValidatedCertificateRequest { certificate } = inner.try_into()?;
        trace!(?certificate, "Handling certificate");
        match self
            .state
            .clone()
            .handle_validated_certificate(certificate)
            .await
        {
            Ok((info, actions)) => {
                Self::log_request_success("handle_validated_certificate", traffic_type);
                self.handle_network_actions(actions);
                Ok(Response::new(info.try_into()?))
            }
            Err(error) => {
                Self::log_request_error(
                    "handle_validated_certificate",
                    traffic_type,
                    &error.error_type(),
                );
                self.log_error(&error, "Failed to handle validated certificate");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_timeout_certificate(
        &self,
        request: Request<api::HandleTimeoutCertificateRequest>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_timeout_certificate(Self::forwarding_request(inner))
                    .await;
            }
        };
        let HandleTimeoutCertificateRequest { certificate } = inner.try_into()?;
        trace!(?certificate, "Handling Timeout certificate");
        match self
            .state
            .clone()
            .handle_timeout_certificate(certificate)
            .await
        {
            Ok((info, _actions)) => {
                Self::log_request_success("handle_timeout_certificate", traffic_type);
                Ok(Response::new(info.try_into()?))
            }
            Err(error) => {
                Self::log_request_error(
                    "handle_timeout_certificate",
                    traffic_type,
                    &error.error_type(),
                );
                self.log_error(&error, "Failed to handle timeout certificate");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_chain_info_query(
        &self,
        request: Request<ChainInfoQuery>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_chain_info_query(Self::forwarding_request(inner))
                    .await;
            }
        };
        let query = inner.try_into()?;
        trace!(?query, "Handling chain info query");
        match self.state.clone().handle_chain_info_query(query).await {
            Ok(info) => {
                Self::log_request_success("handle_chain_info_query", traffic_type);
                Ok(Response::new(info.try_into()?))
            }
            Err(error) => {
                Self::log_request_error(
                    "handle_chain_info_query",
                    traffic_type,
                    &error.error_type(),
                );
                self.log_error(&error, "Failed to handle chain info query");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn download_pending_blob(
        &self,
        request: Request<PendingBlobRequest>,
    ) -> Result<Response<PendingBlobResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let routed_chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(routed_chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .download_pending_blob(Self::forwarding_request(inner))
                    .await;
            }
        };
        let (chain_id, blob_id) = inner.try_into()?;
        trace!(?blob_id, "Download pending blob");
        match self
            .state
            .clone()
            .download_pending_blob(chain_id, blob_id)
            .await
        {
            Ok(blob) => {
                Self::log_request_success("download_pending_blob", traffic_type);
                Ok(Response::new(blob.content().clone().try_into()?))
            }
            Err(error) => {
                Self::log_request_error("download_pending_blob", traffic_type, &error.error_type());
                self.log_error(&error, "Failed to download pending blob");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_pending_blob(
        &self,
        request: Request<HandlePendingBlobRequest>,
    ) -> Result<Response<ChainInfoResult>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let inner = request.into_inner();
        let routed_chain_id = GrpcProxyable::chain_id(&inner)
            .ok_or_else(|| Status::invalid_argument("missing chain ID"))?;
        let _route_guard = match self.route(routed_chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_pending_blob(Self::forwarding_request(inner))
                    .await;
            }
        };
        let (chain_id, blob_content) = inner.try_into()?;
        let blob = Blob::new(blob_content);
        let blob_id = blob.id();
        trace!(?blob_id, "Handle pending blob");
        match self.state.clone().handle_pending_blob(chain_id, blob).await {
            Ok(info) => {
                Self::log_request_success("handle_pending_blob", traffic_type);
                Ok(Response::new(info.try_into()?))
            }
            Err(error) => {
                Self::log_request_error("handle_pending_blob", traffic_type, &error.error_type());
                self.log_error(&error, "Failed to handle pending blob");
                Ok(Response::new(NodeError::from(error).try_into()?))
            }
        }
    }

    #[instrument(
        target = "grpc_server",
        skip_all,
        err,
        fields(
            nickname = self.state.nickname(),
            chain_id = ?request.get_ref().chain_id()
        )
    )]
    async fn handle_cross_chain_request(
        &self,
        request: Request<CrossChainRequest>,
    ) -> Result<Response<()>, Status> {
        let traffic_type = Self::get_traffic_type(&request);
        let forwarded = Self::is_forwarded(&request);
        let cross_chain_request: linera_core::data_types::CrossChainRequest =
            request.into_inner().try_into()?;
        // Note: routing uses the domain-level `target_chain_id`, i.e. the chain
        // whose state this request modifies (the recipient for updates, the
        // sender for confirmations).
        let target_chain_id = cross_chain_request.target_chain_id();
        let _route_guard = match self.route(target_chain_id, forwarded).await? {
            Routed::Local(guard) => guard,
            Routed::Forward(mut client) => {
                return client
                    .handle_cross_chain_request(Self::forwarding_request(
                        cross_chain_request.try_into()?,
                    ))
                    .await;
            }
        };
        trace!(?cross_chain_request, "Handling cross-chain request");
        match self
            .state
            .clone()
            .handle_cross_chain_request(cross_chain_request)
            .await
        {
            Ok(actions) => {
                Self::log_request_success("handle_cross_chain_request", traffic_type);
                self.handle_network_actions(actions)
            }
            Err(error) => {
                Self::log_request_error(
                    "handle_cross_chain_request",
                    traffic_type,
                    &error.error_type(),
                );
                self.log_error(&error, "Failed to handle cross-chain request");
            }
        }
        Ok(Response::new(()))
    }

    #[instrument(target = "grpc_server", skip_all, err, fields(nickname = self.state.nickname()))]
    async fn release_chain(
        &self,
        request: Request<api::ShardAssignmentUpdate>,
    ) -> Result<Response<()>, Status> {
        let (chain_id, target_shard) = self.parse_assignment(request.into_inner())?;
        if target_shard == self.shard_id {
            return Err(Status::invalid_argument(format!(
                "cannot release chain {chain_id} to the shard that currently runs it \
                 ({target_shard})"
            )));
        }
        info!(
            %chain_id,
            from_shard = self.shard_id,
            to_shard = target_shard,
            "releasing chain"
        );
        // Flip the routing entry. This waits for all in-flight requests for the
        // chain to complete; afterwards, new requests are forwarded to the
        // target shard instead of being processed locally.
        self.router.assign(chain_id, target_shard).await;
        // Wait for any remaining (detached) writes and drop the chain's
        // in-memory state; the target worker reloads it from shared storage.
        self.state.drain_chain(chain_id).await;
        self.persist_assignments().await?;
        info!(%chain_id, to_shard = target_shard, "chain released");
        Ok(Response::new(()))
    }

    #[instrument(target = "grpc_server", skip_all, err, fields(nickname = self.state.nickname()))]
    async fn acquire_chain(
        &self,
        request: Request<api::ShardAssignmentUpdate>,
    ) -> Result<Response<()>, Status> {
        let (chain_id, target_shard) = self.parse_assignment(request.into_inner())?;
        if target_shard != self.shard_id {
            return Err(Status::invalid_argument(format!(
                "acquire request for shard {target_shard} sent to shard {}",
                self.shard_id
            )));
        }
        info!(%chain_id, shard = self.shard_id, "acquiring chain");
        self.router.assign(chain_id, target_shard).await;
        self.persist_assignments().await?;
        Ok(Response::new(()))
    }

    #[instrument(target = "grpc_server", skip_all, err, fields(nickname = self.state.nickname()))]
    async fn update_shard_assignment(
        &self,
        request: Request<api::ShardAssignmentUpdate>,
    ) -> Result<Response<()>, Status> {
        let (chain_id, target_shard) = self.parse_assignment(request.into_inner())?;
        trace!(%chain_id, shard = target_shard, "updating shard assignment");
        self.router.assign(chain_id, target_shard).await;
        Ok(Response::new(()))
    }

    #[instrument(target = "grpc_server", skip_all, err, fields(nickname = self.state.nickname()))]
    async fn get_shard_assignment(
        &self,
        request: Request<api::ChainId>,
    ) -> Result<Response<api::ShardAssignmentUpdate>, Status> {
        let chain_id: ChainId = request.into_inner().try_into()?;
        let shard_id = self.router.shard_for(chain_id).await;
        Ok(Response::new(api::ShardAssignmentUpdate {
            chain_id: Some(chain_id.into()),
            shard_id: shard_id as u64,
        }))
    }
}

/// Types which are proxyable and expose the appropriate methods to be handled
/// by the `GrpcProxy`
pub trait GrpcProxyable {
    /// Returns the chain ID this message is destined for, if any.
    fn chain_id(&self) -> Option<ChainId>;
}

impl GrpcProxyable for BlockProposal {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for LiteCertificate {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for api::HandleConfirmedCertificateRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for api::HandleTimeoutCertificateRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for api::HandleValidatedCertificateRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for ChainInfoQuery {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for PendingBlobRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for HandlePendingBlobRequest {
    fn chain_id(&self) -> Option<ChainId> {
        self.chain_id.clone()?.try_into().ok()
    }
}

impl GrpcProxyable for CrossChainRequest {
    fn chain_id(&self) -> Option<ChainId> {
        use super::api::cross_chain_request::Inner;

        match self.inner.as_ref()? {
            Inner::UpdateRecipient(api::UpdateRecipient { recipient, .. })
            | Inner::ConfirmUpdatedRecipient(api::ConfirmUpdatedRecipient { recipient, .. })
            | Inner::RevertConfirm(api::RevertConfirm { recipient, .. }) => {
                recipient.clone()?.try_into().ok()
            }
        }
    }
}
