use crate::{config::Upstream, middleware::GatewayError};
use hyper::client::Client;
use hyper::client::HttpConnector;
use hyper::{header::HeaderValue, Body, Request, Response, Uri};
use hyper_rustls::HttpsConnector;
use rustls::ClientConfig;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tower::Service;
use tracing::{event, Level};

lazy_static::lazy_static! {

    static ref HTTP_REQ_INPROGRESS: prometheus::IntGaugeVec = prometheus::register_int_gauge_vec!(
        "gateway_requests_in_progress",
        "Request in progress count",
        &["service", "upstream", "version"]
    ).unwrap();

}

#[derive(Debug, Clone)]
pub struct ProxyHandler {
    service_id: String,
    upstream_id: String,
    upstream: String,
    version: String,
    timeout: Duration,
    client: Client<HttpsConnector<HttpConnector>, Body>,
}

impl ProxyHandler {
    pub fn new(service_id: &str, upstream: &Upstream, timeout: u32) -> Self {
        let mut connector = HttpConnector::new();
        let timeout = Duration::from_secs(timeout as u64);
        connector.set_connect_timeout(Some(timeout));
        connector.set_keepalive(Some(Duration::from_secs(30)));

        let mut tls_config = ClientConfig::new();
        tls_config.root_store = match rustls_native_certs::load_native_certs() {
            Ok(store) => store,
            Err((Some(store), err)) => {
                log::warn!("Could not load all certificates: {:?}", err);
                store
            }
            Err((None, err)) => Err(err).expect("cannot access native cert store"),
        };
        if tls_config.root_store.is_empty() {
            panic!("no CA certificates found");
        }

        let tls = HttpsConnector::from((connector, tls_config));
        let client = Client::builder()
            .pool_idle_timeout(timeout)
            .build::<_, Body>(tls);

        ProxyHandler {
            service_id: String::from(service_id),
            client,
            timeout,
            upstream: upstream.target.clone(),
            upstream_id: upstream.id.clone(),
            version: upstream.version.clone(),
        }
    }

    fn alter_request(req: Request<Body>, endpoint: &str) -> Request<Body> {
        let (mut parts, body) = req.into_parts();
        parts.version = hyper::http::Version::HTTP_11;
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|pq| pq.as_str())
            .unwrap_or("/");
        let path = path_and_query.strip_prefix("/").unwrap_or("/");
        let path_left = if let Some(offset) = path.find("/") {
            let (_service_id, path_left) = path.split_at(offset);
            path_left
        } else {
            ""
        };
        let mut new_uri = String::from(endpoint.trim_end_matches('/'));
        new_uri.push_str(path_left);

        parts.uri = new_uri.parse::<Uri>().unwrap();
        Request::from_parts(parts, body)
    }
}

impl Service<Request<Body>> for ProxyHandler {
    type Response = Response<Body>;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _c: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let req = ProxyHandler::alter_request(req, &self.upstream);
        event!(Level::DEBUG, "{:?}", req.uri());
        let upstream_id = self.upstream_id.to_string();
        let version = self.version.to_string();
        let service_id = self.service_id.clone();
        HTTP_REQ_INPROGRESS
            .with_label_values(&[&service_id, &upstream_id, &version])
            .inc();

        let sleep = tokio::time::sleep(self.timeout.clone());
        let fut = self.client.request(req);
        Box::pin(async move {
            let result: Result<Response<Body>, GatewayError> = tokio::select! {
                resp = fut => {
                    Ok(resp?)
                },
                _ = sleep => {
                    Err(GatewayError::TimeoutError)
                },
            };

            HTTP_REQ_INPROGRESS
                .with_label_values(&[&service_id, &upstream_id, &version])
                .dec();

            let mut resp = result?;
            let header = resp.headers_mut();
            let us_id = HeaderValue::from_str(&upstream_id).unwrap();
            let us_version = HeaderValue::from_str(&version).unwrap();
            header.append("X-UPSTREAM-ID", us_id);
            header.append("X-UPSTREAM-VERSION", us_version);
            Ok(resp)
        })
    }
}
