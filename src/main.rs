use anyhow::Result;
use clap::Parser;
use forwarding_relayer::{Config, Relayer};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Parse configuration
    let config = Config::parse();

    // Create and run relayer
    let mut relayer = Relayer::new(config).await?;
    relayer.run().await
}
