use clap::{App, Arg};
use hyper::server::conn::AddrIncoming;
use hyper::service::make_service_fn;
use hyper::Server;
use hyperapi::config::ConfigSource;
use hyperapi::proxy::{GatewayServer, TlsAcceptor, TlsConfigBuilder};
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use tracing::{event, Level};
use tracing_bunyan_formatter::{BunyanFormattingLayer, JsonStorageLayer};
use tracing_log::LogTracer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::{filter::EnvFilter, Registry};

#[tokio::main]
async fn main() {
    // setup logging
    LogTracer::init().expect("Unable to setup log tracer!");
    let app_name = concat!(env!("CARGO_PKG_NAME"), "-", env!("CARGO_PKG_VERSION")).to_string();
    let (non_blocking_writer, _guard) = tracing_appender::non_blocking(std::io::stdout());
    let bunyan_formatting_layer = BunyanFormattingLayer::new(app_name, non_blocking_writer);
    let subscriber = Registry::default()
        .with(EnvFilter::new("INFO"))
        .with(JsonStorageLayer)
        .with(bunyan_formatting_layer);
    tracing::subscriber::set_global_default(subscriber).unwrap();

    let matches = App::new("hyperapi")
        .version("0.2.4")
        .author("Leric Zhang <leric.zhang@gmail.com>")
        .about("The gateway to API")
        .arg(
            Arg::new("config")
                .required(true)
                .takes_value(true)
                .short('c')
                .long("config")
                .value_name("FILE")
                .help("Set config file path"),
        )
        .arg(
            Arg::new("listen")
                .required(true)
                .takes_value(true)
                .short('L')
                .long("listen")
                .help("Listening port"),
        )
        .arg(
            Arg::new("cert_file")
                .takes_value(true)
                .long("cert_file")
                .default_value("")
                .help("HTTPS cert file"),
        )
        .arg(
            Arg::new("key_file")
                .takes_value(true)
                .long("key_file")
                .default_value("")
                .help("HTTPS private key file"),
        )
        .get_matches();
    let config = matches.value_of("config").unwrap();
    let listen = matches.value_of("listen").unwrap();
    let cert_file = matches.value_of("cert_file").unwrap();
    let key_file = matches.value_of("key_file").unwrap();

    let config_source = ConfigSource::new(config.into());
    let addr = listen.parse().expect("Invalid listen address");

    let server = GatewayServer::new(config_source);
    let server = Arc::new(Mutex::new(server));

    let incoming = AddrIncoming::bind(&addr).unwrap();
    if cert_file != "" && key_file != "" {
        event!(Level::INFO, "Starting https gateway edge server");
        let make_svc = make_service_fn(|_| {
            let handler = {
                let lock = server.lock().expect("GatewayServer status error");
                lock.make_service()
            };
            async move { Ok::<_, Infallible>(handler) }
        });
        let config = TlsConfigBuilder::new()
            .key_path(key_file)
            .cert_path(cert_file)
            .build()
            .expect("Fail to load TLS certificates");
        let acceptor = TlsAcceptor::new(config, incoming);
        let server = Server::builder(acceptor).serve(make_svc);
        server.await.expect("Server failed to start");
    } else {
        event!(Level::INFO, "Starting http gateway edge server");
        let make_svc = make_service_fn(|_| {
            let handler = {
                let lock = server.lock().expect("GatewayServer status error");
                lock.make_service()
            };
            async move { Ok::<_, Infallible>(handler) }
        });
        let server = Server::builder(incoming).serve(make_svc);
        server.await.expect("Server failed to start");
    }
}
