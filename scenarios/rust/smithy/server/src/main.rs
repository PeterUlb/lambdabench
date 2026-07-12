//! Scenario "smithy": Smithy server SDK hosted behind a Lambda handler, with NO
//! AWS call.
//!
//! `GetMenu` increments a shared in-memory counter (exercising the framework's
//! `AddExtensionLayer` + `Extension<>` shared-state path, as a real Smithy
//! server would) and returns a constant menu. No AWS client construction or
//! network I/O, so the cold-start delta versus `hello` isolates the pure Smithy
//! server framework overhead.
//!
//! The bencher drives this with a synthetic API Gateway v2 HTTP event (GET
//! /menu) adapted by the SSDK's `LambdaHandler`; no live API Gateway is
//! involved.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use coffeeshop_server_sdk::server::{routing::LambdaHandler, AddExtensionLayer, Extension};
use coffeeshop_server_sdk::{
    error, input,
    model::{CoffeeItem, CoffeeType},
    output, CoffeeShop, CoffeeShopConfig,
};

/// Shared state: a request counter, reached via the framework extension layer
/// and reused across warm invocations.
#[derive(Debug, Default)]
struct State {
    requests: AtomicU64,
}

/// `GetMenu` bumps the shared counter and returns a constant menu item.
async fn get_menu(
    _input: input::GetMenuInput,
    state: Extension<Arc<State>>,
) -> Result<output::GetMenuOutput, error::GetMenuError> {
    let n = state.requests.fetch_add(1, Ordering::Relaxed) + 1;
    let item = CoffeeItem::builder()
        .r#type(CoffeeType::Drip)
        .description(format!("lambdabench #{n}"))
        .build()
        .expect("CoffeeItem is fully specified");
    Ok(output::GetMenuOutput {
        items: Some(vec![item]),
    })
}

/// Minimal stub: the benchmark only invokes `GetMenu`, but the service builder
/// requires every operation to be registered.
async fn get_order(
    input: input::GetOrderInput,
    _state: Extension<Arc<State>>,
) -> Result<output::GetOrderOutput, error::GetOrderError> {
    Err(error::OrderNotFound {
        order_id: Some(input.id().clone()),
        message: Some("not implemented in benchmark".to_string()),
    }
    .into())
}

/// Minimal stub for the same reason as `get_order`.
async fn create_order(
    input: input::CreateOrderInput,
    _state: Extension<Arc<State>>,
) -> Result<output::CreateOrderOutput, error::CreateOrderError> {
    use coffeeshop_server_sdk::model::{self, OrderStatus};
    Ok(output::CreateOrderOutput {
        id: model::Uuid::try_from("00000000-0000-0000-0000-000000000000".to_string())
            .expect("valid uuid"),
        coffee_type: input.coffee_type,
        status: OrderStatus::InProgress,
    })
}

#[tokio::main]
async fn main() {
    let config = CoffeeShopConfig::builder()
        .layer(AddExtensionLayer::new(Arc::new(State::default())))
        .build();

    let app = CoffeeShop::builder(config)
        .get_menu(get_menu)
        .get_order(get_order)
        .create_order(create_order)
        .build()
        .expect("failed to build CoffeeShop service");

    let handler = LambdaHandler::new(app);
    if let Err(err) = lambda_http::run(handler).await {
        eprintln!("lambda error: {err}");
    }
}
