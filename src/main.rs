use anyhow::Result;
use clap::Parser;
use forwarding_relayer::{Backend, Cli, Command, Relayer};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Parse CLI
    let cli = Cli::parse();

    match cli.command {
        Command::Relayer(config) => {
            // Create and run relayer
            let mut relayer = Relayer::new(config).await?;
            relayer.run().await
        }
        Command::Backend(config) => {
            // Create and run backend
            let backend = Backend::new(config.port, config.db_path)?;
            backend.serve().await
        }
        Command::DeriveAddress {
            dest_domain,
            dest_recipient,
        } => {
            // Derive forwarding address
            let address =
                forwarding_relayer::derive_forwarding_address(dest_domain, &dest_recipient)?;
            println!("{}", address);
            Ok(())
        }
        Command::DerivePrivateKey { mnemonic } => {
            // Derive private key from mnemonic
            let private_key = forwarding_relayer::derive_private_key_from_mnemonic(&mnemonic)?;
            println!("{}", private_key);
            Ok(())
        }
    }
}
