//! The server, which binds the Engine API port and runs the proxy with the background work.

use std::{
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    http::StatusCode,
    routing::{get, post},
    serve::ListenerExt,
};
use metrics_exporter_prometheus::PrometheusHandle;
use tokio::{net::TcpListener, time::sleep};
use tokio_util::sync::CancellationToken;
use tower_http::catch_panic::CatchPanicLayer;
use tracing::info;

use crate::{
    config::Config, engine::proxy::Proxy, observability::metrics,
    proof::dispatcher::ProofDispatcher,
};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// The server, bound to its port.
pub struct Server {
    listener: TcpListener,
    router: Router,
    dispatcher: Arc<ProofDispatcher>,
}

impl Server {
    /// Reads the validator keystore, starts the background work, and binds the Engine API port.
    /// `prometheus` renders `/metrics`.
    pub async fn bind(config: Config, prometheus: PrometheusHandle) -> anyhow::Result<Self> {
        metrics::describe();
        let upkeep = prometheus.clone();

        let dispatcher = Arc::new(ProofDispatcher::new(&config));
        dispatcher.start(&config).await?;
        dispatcher.spawn(async move {
            loop {
                sleep(Duration::from_secs(5)).await;
                upkeep.run_upkeep();
            }
        });

        let proxy = Proxy::new(config.el_engine_endpoint.clone(), dispatcher.clone());
        // A panic in the handler returns 500 and keeps the connection of the CL.
        let mut router = Router::new()
            .route("/", post(Proxy::handle))
            .layer(CatchPanicLayer::new())
            .with_state(Arc::new(proxy))
            .route("/health", get(StatusCode::OK))
            .route("/metrics", get(move || async move { prometheus.render() }));
        if let Some(dashboard) = &dispatcher.dashboard {
            router = router.merge(dashboard.clone().router());
        }

        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.port)).await?;
        info!(port = listener.local_addr()?.port(), el = %config.el_engine_endpoint, cl = %config.cl_beacon_endpoint, "engine api listening");

        Ok(Self {
            listener,
            router,
            dispatcher,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.listener.local_addr().expect("the listener is bound")
    }

    /// Serves until `shutdown`, then gives the requests in flight `SHUTDOWN_GRACE` to end.
    pub async fn run(self, shutdown: CancellationToken) -> anyhow::Result<()> {
        let listener = self
            .listener
            .tap_io(|stream| drop(stream.set_nodelay(true)));
        let serve = axum::serve(listener, self.router)
            .with_graceful_shutdown(shutdown.clone().cancelled_owned());
        let result = tokio::select! {
            result = serve => result.map_err(Into::into),
            // An open stream, as the one of the dashboard, does not end by itself.
            () = async { shutdown.cancelled().await; sleep(SHUTDOWN_GRACE).await } => Ok(()),
        };
        self.dispatcher.shutdown().await;
        result
    }
}
