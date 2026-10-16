// One-off probe caller: times sequential GETs to the ECS Express endpoint from
// inside a Lambda sandbox.
//
// A minimal custom-runtime loop (no lambda_runtime crate), same shape as
// oneoff/rust-sdk-init. The HTTP client is built once per process, so every
// request after the process's first reuses the pooled TLS connection. The
// event is a JSON number `n`: the invocation makes `n` timed GETs and returns
// each wall-clock ms (send through full body read), plus the negotiated HTTP
// version and, on the process's first invocation, the first request's ms
// (fresh TCP + TLS connection) separately, not among the samples.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Instant;

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

/// One timed GET; panics (failing the invocation loudly) on anything but `200 hello`.
async fn timed_get(client: &reqwest::Client, url: &str) -> (f64, String) {
    let start = Instant::now();
    let response = client.get(url).send().await.expect("GET");
    let version = format!("{:?}", response.version());
    let status = response.status();
    let body = response.bytes().await.expect("read body");
    let ms = start.elapsed().as_secs_f64() * 1e3;
    assert!(
        status == 200 && &body[..] == b"hello",
        "unexpected response: {status} {body:?}"
    );
    (ms, version)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let url = std::env::var("TARGET_URL").expect("TARGET_URL not set");
    let client = reqwest::Client::new();
    let mut first_ms = None;
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
        let n: usize = next
            .split_once("\r\n\r\n")
            .map(|(_, event)| event.trim())
            .and_then(|event| event.parse().ok())
            .expect("event must be a JSON number");

        let first = if first_ms.is_none() {
            let (ms, _) = timed_get(&client, &url).await;
            first_ms = Some(ms);
            format!("{ms:.3}")
        } else {
            "null".to_string()
        };
        let mut version = String::new();
        let mut samples = Vec::with_capacity(n);
        for _ in 0..n {
            let (ms, v) = timed_get(&client, &url).await;
            version = v;
            samples.push(format!("{ms:.3}"));
        }
        let body = format!(
            "{{\"version\":\"{version}\",\"first_ms\":{first},\"samples\":[{}]}}",
            samples.join(",")
        );
        request(
            &api,
            "POST",
            &format!("/2018-06-01/runtime/invocation/{request_id}/response"),
            &body,
        );
    }
}
