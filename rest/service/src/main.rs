mod v0;

use actix_service::ServiceFactory;
use actix_web::{
    dev::{MessageBody, ServiceRequest, ServiceResponse},
    middleware,
    App,
    HttpServer,
};
use rustls::{
    internal::pemfile::{certs, rsa_private_keys},
    NoClientAuth,
    ServerConfig,
};
use std::io::BufReader;
use structopt::StructOpt;

#[derive(Debug, StructOpt)]
pub(crate) struct CliArgs {
    /// The bind address for the REST interface (with HTTPS)
    /// Default: 0.0.0.0:8080
    #[structopt(long, default_value = "0.0.0.0:8080")]
    https: String,
    /// The bind address for the REST interface (with HTTP)
    #[structopt(long)]
    http: Option<String>,
    /// The Nats Server URL or address to connect to
    /// Default: nats://0.0.0.0:4222
    #[structopt(long, short, default_value = "nats://0.0.0.0:4222")]
    nats: String,

    /// Trace rest requests to the Jaeger endpoint agent
    #[structopt(long, short)]
    jaeger: Option<String>,
}

use actix_web_opentelemetry::RequestTracing;
use opentelemetry::{
    global,
    sdk::{propagation::TraceContextPropagator, trace::Tracer},
};
use opentelemetry_jaeger::Uninstall;

fn init_tracing() -> Option<(Tracer, Uninstall)> {
    if let Ok(filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    } else {
        tracing_subscriber::fmt().with_env_filter("info").init();
    }
    if let Some(agent) = CliArgs::from_args().jaeger {
        tracing::info!("Starting jaeger trace pipeline at {}...", agent);
        // Start a new jaeger trace pipeline
        global::set_text_map_propagator(TraceContextPropagator::new());
        let (_tracer, _uninstall) = opentelemetry_jaeger::new_pipeline()
            .with_agent_endpoint(agent)
            .with_service_name("rest-server")
            .install()
            .expect("Jaeger pipeline install error");
        Some((_tracer, _uninstall))
    } else {
        None
    }
}

/// Extension trait for actix-web applications.
pub trait OpenApiExt<T, B> {
    /// configures the App with this version's handlers and openapi generation
    fn configure_api(
        self,
        config: &dyn Fn(actix_web::App<T, B>) -> actix_web::App<T, B>,
    ) -> actix_web::App<T, B>;
}

impl<T, B> OpenApiExt<T, B> for actix_web::App<T, B>
where
    B: MessageBody,
    T: ServiceFactory<
        Config = (),
        Request = ServiceRequest,
        Response = ServiceResponse<B>,
        Error = actix_web::Error,
        InitError = (),
    >,
{
    fn configure_api(
        self,
        config: &dyn Fn(actix_web::App<T, B>) -> actix_web::App<T, B>,
    ) -> actix_web::App<T, B> {
        config(self)
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // need to keep the jaeger pipeline tracer alive, if enabled
    let _tracer = init_tracing();

    mbus_api::message_bus_init(CliArgs::from_args().nats).await;

    // dummy certificates
    let mut config = ServerConfig::new(NoClientAuth::new());
    let cert_file = &mut BufReader::new(
        &std::include_bytes!("../../certs/rsa/user.chain")[..],
    );
    let key_file = &mut BufReader::new(
        &std::include_bytes!("../../certs/rsa/user.rsa")[..],
    );
    let cert_chain = certs(cert_file).unwrap();
    let mut keys = rsa_private_keys(key_file).unwrap();
    config.set_single_cert(cert_chain, keys.remove(0)).unwrap();

    let server = HttpServer::new(move || {
        App::new()
            .wrap(RequestTracing::new())
            .wrap(middleware::Logger::default())
            .configure_api(&v0::configure_api)
    })
    .bind_rustls(CliArgs::from_args().https, config)?;
    if let Some(http) = CliArgs::from_args().http {
        server.bind(http)?
    } else {
        server
    }
    .run()
    .await
}
