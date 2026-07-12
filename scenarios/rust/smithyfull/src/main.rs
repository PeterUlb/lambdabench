//! Scenario "smithyfull": the realistic shape, the Smithy server framework
//! hosting a handler that does a real AWS write flow.
//!
//! The benchmark invokes `CreateOrder`: the server SDK deserializes and
//! validates the request body, the handler runs a real write flow (KMS-encrypt
//! a signature, DDB `PutItem` the order, S3 `PutObject` a receipt), and the SDK
//! serializes a constraint-validated response. This represents a typical
//! production request handler: framework plus multiple AWS clients plus real
//! (de)serialization. It is read by direct comparison with the other scenarios,
//! not by subtracting them (the layers do not cleanly add up; see config.rs).

use std::sync::Arc;

use aws_sdk_dynamodb::types::AttributeValue;
use coffeeshop_server_sdk::server::{routing::LambdaHandler, AddExtensionLayer, Extension};
use coffeeshop_server_sdk::{
    error, input,
    model::{CoffeeItem, CoffeeType},
    output, CoffeeShop, CoffeeShopConfig,
};

/// Environment variables injected by the bencher at deploy time.
const ENV_TABLE: &str = "LAMBDABENCH_TABLE";
const ENV_KMS_KEY: &str = "LAMBDABENCH_KMS_KEY_ID";
const ENV_BUCKET: &str = "LAMBDABENCH_BUCKET";
const ENV_ORDER_PK: &str = "LAMBDABENCH_ORDER_PK";
const ENV_RECEIPT_KEY: &str = "LAMBDABENCH_RECEIPT_KEY";

/// Shared state: the three AWS clients + resource ids, built once at init and
/// reached by handlers via the framework extension layer.
#[derive(Debug)]
struct State {
    ddb: aws_sdk_dynamodb::Client,
    kms: aws_sdk_kms::Client,
    s3: aws_sdk_s3::Client,
    table: String,
    kms_key_id: String,
    bucket: String,
    order_pk: String,
    receipt_key: String,
}

/// Trivial stub: the benchmark exercises `CreateOrder`, not `GetMenu`.
async fn get_menu(
    _input: input::GetMenuInput,
    _state: Extension<Arc<State>>,
) -> Result<output::GetMenuOutput, error::GetMenuError> {
    let item = CoffeeItem::builder()
        .r#type(CoffeeType::Drip)
        .description("lambdabench".to_string())
        .build()
        .expect("CoffeeItem is fully specified");
    Ok(output::GetMenuOutput {
        items: Some(vec![item]),
    })
}

/// The realistic CreateOrder write flow: KMS-encrypt a signature, DDB PutItem
/// the order, S3 PutObject a receipt. Fixed keys, so repeated invocations
/// overwrite (idempotent; no data accumulation across the benchmark).
async fn run_work(
    state: &State,
    coffee_type: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 1. KMS Encrypt a small "signature" payload for the order.
    let kms = state
        .kms
        .encrypt()
        .key_id(&state.kms_key_id)
        .plaintext(aws_sdk_kms::primitives::Blob::new(
            format!("order:{coffee_type}").into_bytes(),
        ))
        .send()
        .await?;
    let signature = kms
        .ciphertext_blob()
        .map(|b| aws_sdk_dynamodb::primitives::Blob::new(b.as_ref().to_vec()));

    // 2. DDB PutItem: write the order (fixed pk, idempotent overwrite).
    let mut put = state
        .ddb
        .put_item()
        .table_name(&state.table)
        .item("pk", AttributeValue::S(state.order_pk.clone()))
        .item("coffeeType", AttributeValue::S(coffee_type.to_string()))
        .item("status", AttributeValue::S("IN_PROGRESS".to_string()));
    if let Some(sig) = signature {
        put = put.item("signature", AttributeValue::B(sig));
    }
    put.send().await?;

    // 3. S3 PutObject: write a receipt (fixed key, idempotent overwrite).
    state
        .s3
        .put_object()
        .bucket(&state.bucket)
        .key(&state.receipt_key)
        .body(aws_sdk_s3::primitives::ByteStream::from(
            format!("receipt for {coffee_type} order").into_bytes(),
        ))
        .send()
        .await?;

    Ok(())
}

/// Minimal stubs to satisfy the service contract.
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

/// `CreateOrder` is the realistic request path the benchmark invokes. The SSDK
/// has already deserialized + validated the input (`coffeeType`, required, enum)
/// before this runs. We perform the three AWS calls, then return a structured
/// order whose `id` is a `@pattern`+`@length`-constrained `Uuid`, so the SSDK
/// runs constraint validation while serializing the response. This exercises
/// the framework's real (de)serialization + validation work, not just routing.
async fn create_order(
    input: input::CreateOrderInput,
    state: Extension<Arc<State>>,
) -> Result<output::CreateOrderOutput, error::CreateOrderError> {
    use coffeeshop_server_sdk::model::{self, OrderStatus};
    // The model gives CreateOrder no internal-error variant (only
    // ValidationException), so an AWS failure panics, surfacing as a Lambda
    // FunctionError the benchmark treats as a hard failure. The happy path always
    // succeeds against the provisioned resources.
    let coffee = input.coffee_type.as_str().to_string();
    run_work(&state, &coffee)
        .await
        .expect("smithyfull AWS write flow failed");
    Ok(output::CreateOrderOutput {
        id: model::Uuid::try_from("00000000-0000-0000-0000-000000000000".to_string())
            .expect("valid uuid"),
        coffee_type: input.coffee_type,
        status: OrderStatus::InProgress,
    })
}

#[tokio::main]
async fn main() {
    // Init phase: resolve config once, build all three clients into shared state.
    // Retries disabled: a throttle/transient must surface as a hard failure, not
    // be silently retried into an inflated Duration. A failed run beats wrong data.
    let aws = aws_config::defaults(aws_config::BehaviorVersion::latest())
        .retry_config(aws_config::retry::RetryConfig::disabled())
        .load()
        .await;
    let state = Arc::new(State {
        ddb: aws_sdk_dynamodb::Client::new(&aws),
        kms: aws_sdk_kms::Client::new(&aws),
        s3: aws_sdk_s3::Client::new(&aws),
        table: std::env::var(ENV_TABLE).expect("LAMBDABENCH_TABLE not set"),
        kms_key_id: std::env::var(ENV_KMS_KEY).expect("LAMBDABENCH_KMS_KEY_ID not set"),
        bucket: std::env::var(ENV_BUCKET).expect("LAMBDABENCH_BUCKET not set"),
        order_pk: std::env::var(ENV_ORDER_PK).expect("LAMBDABENCH_ORDER_PK not set"),
        receipt_key: std::env::var(ENV_RECEIPT_KEY).expect("LAMBDABENCH_RECEIPT_KEY not set"),
    });

    let config = CoffeeShopConfig::builder()
        .layer(AddExtensionLayer::new(state))
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
