// Init sleeps INIT_MS before the runtime asks for the first event. A payload of {"hang": true}
// sleeps past any timeout, so the invoke times out and the next one pays a suppressed init.
// The hang leaves a marker in /tmp (which survives the reset), so that re-init sleeps
// REINIT_MS instead: a short first cold start, then a re-init of any length.
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use lambda_runtime::{Error, LambdaEvent, service_fn};
use serde_json::{Value, json};

const MARKER: &str = "/tmp/hung";

fn env_ms(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let init_id = format!(
        "{:x}",
        SystemTime::now().duration_since(UNIX_EPOCH)?.subsec_nanos()
    );
    let start = Instant::now();
    let reinit = env_ms("REINIT_MS").filter(|_| std::path::Path::new(MARKER).exists());
    std::thread::sleep(Duration::from_millis(
        reinit.or(env_ms("INIT_MS")).unwrap_or(0),
    ));
    let init_ms = start.elapsed().as_millis() as u64;

    lambda_runtime::run(service_fn(move |event: LambdaEvent<Value>| {
        let init_id = init_id.clone();
        async move {
            if event.payload.get("hang").and_then(Value::as_bool) == Some(true) {
                std::fs::write(MARKER, "")?;
                tokio::time::sleep(Duration::from_secs(900)).await;
            }
            Ok::<_, Error>(json!({ "initId": init_id, "initMs": init_ms }))
        }
    }))
    .await
}
