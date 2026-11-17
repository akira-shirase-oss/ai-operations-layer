//! A host's first operation, end to end: start the client, record one operation with a
//! model step that is retried and a publish step, finish it, and shut down within a
//! deadline. Run against a local collector (README, "Run it locally"):
//!
//! ```sh
//! export AIOPS_ENDPOINT=http://127.0.0.1:57400
//! export AIOPS_INGEST_KEY=$(cargo run -q -p aiops-server -- admin create-ingest-key --project pilot --name example)
//! cargo run -p aiops-sdk --example first_operation
//! ```
//!
//! The operation then appears in the console under request ID `example-1`. With the
//! collector down the example still finishes, and says how many events it dropped.

use std::time::Duration;

use aiops_sdk::{Client, Config, FailureClass, Finish, StepKind, UncertainReason};

#[tokio::main]
async fn main() {
    let (Ok(endpoint), Ok(key)) = (
        std::env::var("AIOPS_ENDPOINT"),
        std::env::var("AIOPS_INGEST_KEY"),
    ) else {
        eprintln!(
            "set AIOPS_ENDPOINT (the collector) and AIOPS_INGEST_KEY (from `aiops-server admin create-ingest-key`)"
        );
        std::process::exit(2);
    };
    let client = match Client::start(&Config::new(endpoint, key, "sdk-example")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("the SDK refused its configuration: {e}");
            std::process::exit(2);
        }
    };

    let op = client
        .operation("publish_article")
        .request_id("example-1")
        .start();
    // A model call that failed before it left the host, then succeeded on attempt 2.
    let generate = op
        .step(StepKind::Model, "generate")
        .model("fake-model-1")
        .start();
    let retry = generate
        .finish(Finish::failed(FailureClass::Transport, "connect_refused"))
        .retry()
        .start();
    retry.finish(
        Finish::succeeded()
            .usage(132, 480)
            .cost_estimate(2_052, "USD", "list-price"),
    );
    // A publish whose reply never came: the effect may have happened.
    let publish = op
        .step(StepKind::Tool, "publish")
        .external_effect(true)
        .start();
    publish.finish(Finish::uncertain(
        UncertainReason::TimeoutAfterSend,
        "publish_timeout",
    ));
    op.finish(Finish::uncertain(
        UncertainReason::TimeoutAfterSend,
        "publish_timeout",
    ));

    let stats = client.shutdown(Duration::from_secs(2)).await;
    println!(
        "emitted {} events: {} accepted, {} dropped (every drop is counted)",
        stats.emitted,
        stats.accepted,
        stats.dropped()
    );
}
