use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::anyhow;
use futures::TryFutureExt;
use health_reporter::{HealthReporter as AdapterHealthReporter, State};
use hyper::header::CONTENT_TYPE;
use hyper::service::make_service_fn;
use hyper::{self, Body, Method, Request, Response};
use metrics_exporter_prometheus::PrometheusHandle;
use readyset_client::query::DeniedQuery;
use readyset_client_metrics::recorded;
use readyset_sql_passes::anonymize::Anonymizer;
use stream_cancel::Valve;
use tokio::net::TcpListener;
use tokio::sync::mpsc::Sender;
use tokio_stream::wrappers::TcpListenerStream;
use tower::Service;

use crate::query_status_cache::QueryStatusCache;

/// Routes requests from an HTTP server to expose metrics data from the adapter.
/// To see the supported http requests and their respective routing, see
/// impl Service<Request<Body>> for NoriaAdapterHttpRouter.
#[derive(Clone)]
pub struct NoriaAdapterHttpRouter {
    /// The address to attempt to listen on.
    pub listen_addr: SocketAddr,
    /// A reference to the QueryStatusCache that is in use by the adapter.
    pub query_cache: &'static QueryStatusCache,
    /// A valve for the http stream to trigger closing.
    pub valve: Valve,
    /// Used to retrieve the current health of the adapter.
    pub health_reporter: AdapterHealthReporter,
    /// Used to communicate externally that a failpoint request has been received and successfully
    /// handled.
    /// Most commonly used to block on further startup action if --wait-for-failpoint is supplied
    /// to the adapter.
    pub failpoint_channel: Option<Arc<Sender<()>>>,

    /// Used to retrieve the prometheus scrape's render as a String when servicing
    /// HTTP requests on /metrics.
    pub prometheus_handle: Option<PrometheusHandle>,
}

impl NoriaAdapterHttpRouter {
    /// Creates a listener object to be used to route requests.
    pub async fn create_listener(&self) -> anyhow::Result<TcpListener> {
        let http_listener = TcpListener::bind(self.listen_addr).await?;
        Ok(http_listener)
    }

    /// Routes requests for a noria adapter http router received on `http_listener`
    /// the service layer of the NoriaAdapterHttpRouter, see
    /// Impl Service<_> for NoriaAdapterHttpRouter.
    pub async fn route_requests(
        router: NoriaAdapterHttpRouter,
        http_listener: TcpListener,
    ) -> anyhow::Result<()> {
        hyper::server::Server::builder(hyper::server::accept::from_stream(
            router.valve.wrap(TcpListenerStream::new(http_listener)),
        ))
        .serve(make_service_fn(move |_| {
            let s = router.clone();
            async move { io::Result::Ok(s) }
        }))
        .map_err(move |e| anyhow!("HTTP server failed, {}", e))
        .await
    }
}

/// Tower service definition to route http requests `Request<Body>` to their
/// responses.
#[allow(clippy::type_complexity)] // No valid re-use to make this into custom type definitions.
impl Service<Request<Body>> for NoriaAdapterHttpRouter {
    type Response = Response<Body>;
    type Error = hyper::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    /// # ReadySet Adapter Endpoints
    ///
    /// The following HTTP endpoints are exposed by the ReadySet Adapter.
    ///
    /// ## Health Check
    ///
    /// Get the health of the adapter. Return 200 code without a response body if the service is
    /// considered healthy or return no response at all if the service is unhealthy.
    ///
    /// "Healthy" _only_ indicates that the HTTP router is active but no further checks are
    /// performed.
    ///
    /// * **URL**
    ///
    ///   `/health`
    ///
    /// * **Method:**
    ///
    ///   `GET`
    ///
    /// * **Success Response:**
    ///
    ///     * **Code:** 200 <br />
    ///
    /// * **Sample Call:**
    ///
    ///   `curl -X GET <adapter>:<adapter-port>/health`
    ///
    /// ## Allow List
    ///
    /// List of SQL queries that will be handled by ReadySet as opposed to being passed through to
    /// the underlying database.
    ///
    /// * **URL**
    ///
    ///   `/allow-list`
    ///
    /// * **Method:**
    ///
    ///   `GET`
    ///
    /// * **Success Response:**
    ///
    ///   Allow list as a JSON Object.
    ///
    ///     * **Code:** 200 <br /> **Content:** `{ ... }`
    ///
    /// * **Error Response:**
    ///
    ///     * **Code:** 500 Internal Server Error <br /> **Content:** `"allow list failed to be
    ///       converted into a json string"`
    ///
    /// * **Sample Call:**
    ///
    ///   `curl -X GET <adapter>:<adapter-port>/allow-list`
    ///
    /// ## Deny List
    ///
    /// List of SQL queries that will _not_ be handled by ReadySet and instead passed through to the
    /// underlying database.
    ///
    /// * **URL**
    ///
    ///   `/deny-list`
    ///
    /// * **Method:**
    ///
    ///   `GET`
    ///
    /// * **Success Response:**
    ///
    ///   Allow list as a JSON Object.
    ///
    ///     * **Code:** 200 <br /> **Content:** `{ ... }`
    ///
    /// * **Error Response:**
    ///
    ///     * **Code:** 500 Internal Server Error <br /> **Content:** `"deny list failed to be
    ///       converted into a json string"`
    ///
    /// * **Sample Call:**
    ///
    ///   `curl -X GET <adapter>:<adapter-port>/deny-list`
    ///
    /// ## Prometheus
    ///
    /// Endpoint for Prometheus metric API calls.
    ///
    /// * **URL**
    ///
    ///   `/metrics`
    ///
    /// * **Method:**
    ///
    ///   `GET`
    ///
    /// * **Success Response:**
    ///
    ///     * **Code:** 200 <br /> **Content:** `{ ... }`
    ///
    /// * **Error Response:**
    ///
    ///   Returns 404 if adapter is run without `--prometheus-metrics` or if the Prometheus exporter
    /// runs into any other type of   error.
    ///
    ///     * **Code:** 404 Not Found <br /> **Content:** `"Prometheus metrics were not enabled. To
    ///       fix this, run the adapter with --prometheus-metrics"`
    ///
    ///   OR
    ///
    ///     * **Code:** 404 Not Found <br />
    ///
    /// * **Sample Call:**
    ///
    ///   `curl -X GET <adapter>:<adapter-port>/metrics`
    ///
    /// * **Notes:**
    ///
    ///   This endpoint is intended to be scraped by Prometheus. For almost all cases you want to
    /// query Prometheus directly to get metrics data.
    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let res = Response::builder()
            // disable CORS to allow use as API server
            .header(hyper::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*");

        metrics::increment_counter!(recorded::ADAPTER_EXTERNAL_REQUESTS);

        match (req.method(), req.uri().path()) {
            #[cfg(feature = "failure_injection")]
            (&Method::GET, "/failpoint") => {
                let tx = self.failpoint_channel.clone();
                Box::pin(async move {
                    let body = hyper::body::to_bytes(req.into_body()).await.unwrap();
                    let contents = match bincode::deserialize(&body) {
                        Err(_) => {
                            return Ok(res
                                .status(400)
                                .header(CONTENT_TYPE, "text/plain")
                                .body(hyper::Body::from(
                                    "body cannot be deserialized into failpoint name and action",
                                ))
                                .unwrap());
                        }
                        Ok(contents) => contents,
                    };
                    let (name, action): (String, String) = contents;
                    let resp = res
                        .status(200)
                        .header(CONTENT_TYPE, "text/plain")
                        .body(hyper::Body::from(
                            ::bincode::serialize(&fail::cfg(name, &action)).unwrap(),
                        ))
                        .unwrap();
                    if let Some(tx) = tx {
                        let _ = tx.send(()).await;
                    }
                    Ok(resp)
                })
            }
            (&Method::GET, "/allow-list") => {
                let query_cache = self.query_cache;
                Box::pin(async move {
                    let allow_list = query_cache.allow_list();
                    let res = match serde_json::to_string(&allow_list) {
                        Ok(json) => res
                            .header(CONTENT_TYPE, "application/json")
                            .body(hyper::Body::from(json)),
                        Err(_) => res.status(500).header(CONTENT_TYPE, "text/plain").body(
                            hyper::Body::from(
                                "allow list failed to be converted into a json string".to_string(),
                            ),
                        ),
                    };
                    Ok(res.unwrap())
                })
            }
            (&Method::GET, "/deny-list") => {
                let query_cache = self.query_cache;
                Box::pin(async move {
                    let mut anonymizer = Anonymizer::new();
                    let deny_list = query_cache
                        .deny_list()
                        .into_iter()
                        .map(|DeniedQuery { query, .. }| {
                            query.to_anonymized_string(&mut anonymizer)
                        })
                        .collect::<Vec<_>>();
                    let res = match serde_json::to_string(&deny_list) {
                        Ok(json) => res
                            .header(CONTENT_TYPE, "application/json")
                            .body(hyper::Body::from(json)),
                        Err(_) => res.status(500).header(CONTENT_TYPE, "text/plain").body(
                            hyper::Body::from(
                                "deny list failed to be converted into a json string".to_string(),
                            ),
                        ),
                    };
                    Ok(res.unwrap())
                })
            }
            (&Method::GET, "/health") => {
                let state = self.health_reporter.health().state;
                Box::pin(async move {
                    let body = format!("Adapter is in {} state", &state).into();
                    let res = match state {
                        State::Healthy | State::ShuttingDown => res
                            .status(200)
                            .header(CONTENT_TYPE, "text/plain")
                            .body(body),
                        _ => res
                            .status(500)
                            .header(CONTENT_TYPE, "text/plain")
                            .body(body),
                    };

                    Ok(res.unwrap())
                })
            }
            (&Method::GET, "/metrics") => {
                let body = self.prometheus_handle.as_ref().map(|x| x.render());
                let res = res.header(CONTENT_TYPE, "text/plain");
                let res = match body {
                    Some(metrics) => res.body(hyper::Body::from(metrics)),
                    None => res
                        .status(404)
                        .body(hyper::Body::from("Prometheus metrics were not enabled. To fix this, run the adapter with --prometheus-metrics".to_string())),
                };
                Box::pin(async move { Ok(res.unwrap()) })
            }
            _ => Box::pin(async move {
                let res = res
                    .status(404)
                    .header(CONTENT_TYPE, "text/plain")
                    .body(hyper::Body::empty());

                Ok(res.unwrap())
            }),
        }
    }
}
