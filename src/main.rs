//! Tengu binary entry point: the CLI is the `tengu` lib (`src/lib.rs`).

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tengu::run().await
}
