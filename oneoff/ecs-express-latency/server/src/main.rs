// One-off probe target: an axum hello world behind ECS Express Mode.
//
// `/` is what the caller times; `/ping` is Express Mode's default ALB health
// check path. Both return the same constant body, so the server does no work
// beyond HTTP parsing.

use axum::{Router, routing::get};

async fn hello() -> &'static str {
    "hello"
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(hello))
        .route("/ping", get(hello));
    // Express Mode's default container port.
    let listener = tokio::net::TcpListener::bind("0.0.0.0:80")
        .await
        .expect("bind :80");
    axum::serve(listener, app).await.expect("serve");
}
