// One-off probe: times `aws_config::load()` variants inside a Lambda sandbox.
//
// A minimal custom-runtime loop (no lambda_runtime crate) so the only setup
// work in the process is the variant under test. PROBE_MODE picks the variant,
// PROBE_PHASE picks where it runs: `init` (before the first /next poll, so in
// the Init phase) or `invoke` (inside the first invocation). The elapsed ms is
// returned as the invocation's response body.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

use aws_config::BehaviorVersion;
use aws_smithy_runtime_api::client::http::{
    HttpClient, HttpConnector, HttpConnectorFuture, HttpConnectorSettings, SharedHttpConnector,
};
use aws_smithy_runtime_api::client::orchestrator::HttpRequest;
use aws_smithy_runtime_api::client::result::ConnectorError;
use aws_smithy_runtime_api::client::runtime_components::RuntimeComponents;

/// HTTP client that never builds a TLS connector, to isolate its cost from `load()`.
#[derive(Debug)]
struct Noop;

impl HttpConnector for Noop {
    fn call(&self, _: HttpRequest) -> HttpConnectorFuture {
        HttpConnectorFuture::ready(Err(ConnectorError::other("noop".into(), None)))
    }
}

impl HttpClient for Noop {
    fn http_connector(
        &self,
        _: &HttpConnectorSettings,
        _: &RuntimeComponents,
    ) -> SharedHttpConnector {
        SharedHttpConnector::new(Noop)
    }
}

/// Runs the variant once and returns the elapsed ms.
///
/// Modes: `default`, `noop` (no-op HTTP client), `static` (static credentials,
/// no provider chain), `certs` (`load_native_certs()` alone), and
/// `<warm>+<timed>`, which runs `<warm>` untimed first, separating the
/// first-call cost from the steady cost.
async fn measure(mode: &str) -> f64 {
    if let Some((warm, timed)) = mode.split_once('+') {
        Box::pin(measure(warm)).await;
        return Box::pin(measure(timed)).await;
    }
    // Same loader settings as scenarios/rust/oneclient.
    let loader = || {
        aws_config::defaults(BehaviorVersion::latest())
            .retry_config(aws_config::retry::RetryConfig::disabled())
    };
    let start = Instant::now();
    match mode {
        "default" => drop(std::hint::black_box(loader().load().await)),
        "noop" => drop(std::hint::black_box(
            loader().http_client(Noop).load().await,
        )),
        "static" => {
            let creds =
                aws_credential_types::Credentials::new("AKID", "SECRET", None, None, "static");
            drop(std::hint::black_box(
                loader().credentials_provider(creds).load().await,
            ))
        }
        "certs" => drop(std::hint::black_box(
            rustls_native_certs::load_native_certs(),
        )),
        other => panic!("unknown PROBE_MODE {other}"),
    }
    start.elapsed().as_secs_f64() * 1e3
}

/// One blocking HTTP/1.1 request to the Lambda runtime API; returns the raw response.
fn request(api: &str, method: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(api).expect("connect to runtime API");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {api}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .expect("write runtime API request");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("read runtime API response");
    response
}

#[tokio::main]
async fn main() {
    let mode = std::env::var("PROBE_MODE").expect("PROBE_MODE not set");
    let in_handler = std::env::var("PROBE_PHASE").as_deref() == Ok("invoke");
    let mut elapsed_ms = if in_handler {
        None
    } else {
        Some(measure(&mode).await)
    };
    let api = std::env::var("AWS_LAMBDA_RUNTIME_API").expect("AWS_LAMBDA_RUNTIME_API not set");
    loop {
        let next = request(&api, "GET", "/2018-06-01/runtime/invocation/next", "");
        let request_id = next
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("lambda-runtime-aws-request-id")
                    .then(|| value.trim().to_string())
            })
            .expect("request id header");
        if elapsed_ms.is_none() {
            elapsed_ms = Some(measure(&mode).await);
        }
        let body = format!("{{\"ms\":{:.3}}}", elapsed_ms.unwrap_or_default());
        request(
            &api,
            "POST",
            &format!("/2018-06-01/runtime/invocation/{request_id}/response"),
            &body,
        );
    }
}
