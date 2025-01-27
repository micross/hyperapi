use crate::config::{ConfigUpdate, ServiceInfo};
use crate::middleware::proxy::ProxyHandler;
use crate::middleware::weighted::WeightedBalance;
use crate::middleware::{CircuitBreakerConfig, CircuitBreakerService};
use crate::middleware::{
    GatewayError, Middleware, MwNextAction, MwPostRequest, MwPreRequest, MwPreResponse,
};
use hyper::header::HeaderValue;
use hyper::{Body, Request, Response};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tower::balance::p2c::Balance;
use tower::discover::ServiceList;
use tower::limit::concurrency::ConcurrencyLimit;
use tower::load::{CompleteOnResponse, Constant, PeakEwmaDiscover, PendingRequestsDiscover};
use tower::load_shed::LoadShed;
use tower::steer::Steer;
use tower::util::{BoxService, ServiceExt};
use tower::Service;
use tracing::{event, Level};

#[derive(Debug)]
pub struct UpstreamMiddleware {
    pub worker_queues: HashMap<String, mpsc::Sender<MwPreRequest>>,
}

impl Default for UpstreamMiddleware {
    fn default() -> Self {
        UpstreamMiddleware {
            worker_queues: HashMap::new(),
        }
    }
}

type BoxedHttpService =
    BoxService<Request<Body>, Response<Body>, Box<dyn std::error::Error + Send + Sync>>;

impl UpstreamMiddleware {
    async fn service_worker(mut rx: mpsc::Receiver<MwPreRequest>, conf: ServiceInfo) {
        let mut service = Self::build_service(&conf);

        while let Some(MwPreRequest {
            context,
            request,
            result,
            ..
        }) = rx.recv().await
        {
            event!(Level::DEBUG, "request {:?}", request.uri());
            if let Ok(px) = service.ready().await {
                let f = px.call(request);
                tokio::spawn(async move {
                    let proxy_resp: Result<
                        Response<Body>,
                        Box<dyn std::error::Error + Send + Sync>,
                    > = f.await;
                    match proxy_resp {
                        Ok(resp) => {
                            let response = MwPreResponse {
                                context,
                                next: MwNextAction::Return(resp),
                            };
                            let _ = result.send(Ok(response));
                        }
                        Err(e) => {
                            if let Some(err) = e.downcast_ref::<GatewayError>() {
                                let _ = result.send(Err(err.clone()));
                            } else {
                                let msg = format!("Upstream error\n{:?}", e);
                                let _ = result.send(Err(GatewayError::UpstreamError(msg)));
                            }
                        }
                    }
                });
            } else {
                let _ = result.send(Err(GatewayError::ServiceNotReady(
                    "Service not ready".into(),
                )));
            }
        }
    }

    fn build_service(conf: &ServiceInfo) -> BoxedHttpService {
        match conf.upstreams.len() {
            0 => {
                panic!("Invalid upstream config");
            }
            1 => {
                let u = conf.upstreams.get(0).unwrap();
                let cb_config = CircuitBreakerConfig {
                    error_threshold: u.error_threshold,
                    error_reset: Duration::from_secs(u.error_reset),
                    retry_delay: Duration::from_secs(u.retry_delay),
                };
                let us = ProxyHandler::new(&conf.service_id, u, conf.timeout);
                let limit = ConcurrencyLimit::new(us, u.max_conn as usize);
                let cb = CircuitBreakerService::new(LoadShed::new(limit), cb_config);
                BoxService::new(LoadShed::new(cb))
            }
            _ => {
                let u = conf.upstreams.get(0).unwrap();
                let cb_config = CircuitBreakerConfig {
                    error_threshold: u.error_threshold,
                    error_reset: Duration::from_secs(u.error_reset),
                    retry_delay: Duration::from_secs(u.retry_delay),
                };
                let list: Vec<
                    Constant<CircuitBreakerService<LoadShed<ConcurrencyLimit<ProxyHandler>>>, u32>,
                > = conf
                    .upstreams
                    .iter()
                    .map(|u| {
                        let us = ProxyHandler::new(&conf.service_id, u, conf.timeout);
                        let limit = ConcurrencyLimit::new(us, u.max_conn as usize);
                        let cb = CircuitBreakerService::new(LoadShed::new(limit), cb_config);
                        Constant::new(cb, u.weight)
                    })
                    .collect();

                if conf.load_balance.eq("hash") {
                    let list: Vec<
                        LoadShed<
                            Constant<
                                CircuitBreakerService<LoadShed<ConcurrencyLimit<ProxyHandler>>>,
                                u32,
                            >,
                        >,
                    > = list.into_iter().map(|s| LoadShed::new(s)).collect();
                    let balance = Steer::new(list, |req: &Request<_>, s: &[_]| {
                        let total = s.len();
                        let default = HeaderValue::from_static("empty");
                        let client_id = req
                            .headers()
                            .get("x-lb-hash")
                            .unwrap_or(&default)
                            .as_bytes();
                        let mut hasher = DefaultHasher::new();
                        Hash::hash_slice(&client_id, &mut hasher);
                        (hasher.finish() as usize) % total
                    });
                    BoxService::new(balance)
                } else if conf.load_balance.eq("load") {
                    let discover = ServiceList::new(list);
                    let load = PeakEwmaDiscover::new(
                        discover,
                        Duration::from_millis(50),
                        Duration::from_secs(1),
                        CompleteOnResponse::default(),
                    );
                    let balance = Balance::new(load);
                    BoxService::new(balance)
                } else if conf.load_balance.eq("conn") {
                    let discover = ServiceList::new(list);
                    let load =
                        PendingRequestsDiscover::new(discover, CompleteOnResponse::default());
                    let balance = Balance::new(load);
                    BoxService::new(balance)
                } else {
                    // weighted random
                    let discover = ServiceList::new(list);
                    let balance = WeightedBalance::new(discover);
                    BoxService::new(balance)
                }
            }
        }
    }
}

impl Middleware for UpstreamMiddleware {
    fn name() -> String {
        "Upstream".into()
    }

    fn post() -> bool {
        false
    }

    fn require_setting() -> bool {
        false
    }

    fn request(&mut self, task: MwPreRequest) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        if let Some(ch) = self.worker_queues.get_mut(&task.context.service_id) {
            let task_ch = ch.clone();
            Box::pin(async move {
                let _ = task_ch.send(task).await;
            })
        } else {
            Box::pin(async {
                let _ = task.result.send(Err(GatewayError::ServiceNotFound(
                    "Invalid Service ID".into(),
                )));
            })
        }
    }

    fn response(&mut self, _task: MwPostRequest) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        panic!("never got here");
    }

    fn config_update(&mut self, update: ConfigUpdate) {
        match update {
            ConfigUpdate::ServiceUpdate(conf) => {
                let (tx, rx) = mpsc::channel(10);
                let service_id = conf.service_id.clone();
                if (&conf.upstreams).len() > 0 {
                    tokio::spawn(async move {
                        Self::service_worker(rx, conf).await;
                    });
                    self.worker_queues.insert(service_id, tx);
                } else {
                    self.worker_queues.remove(&service_id);
                }
            }
            ConfigUpdate::ServiceRemove(sid) => {
                self.worker_queues.remove(&sid);
            }
            _ => {}
        }
    }
}
