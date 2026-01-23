use anyhow::Result;
use forwarding_relayer::{derive_forwarding_address, Backend, ForwardingRequest};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .init();

    // Create backend server with SQLite storage
    let backend = Backend::new(8080, "storage/backend.db".into())?;
    let state = backend.state();

    // Add the actual E2E test forwarding request from Makefile
    let dest_domain = 1234;
    let dest_recipient = "0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d";
    let forward_addr = derive_forwarding_address(dest_domain, dest_recipient)?;

    let request = ForwardingRequest {
        id: "e2e-test-1".to_string(),
        forward_addr: forward_addr.clone(),
        dest_domain,
        dest_recipient: dest_recipient.to_string(),
        status: "pending".to_string(),
        created_at: Some(chrono::Utc::now().to_rfc3339()),
    };

    state.add_request(request)?;

    println!("Added E2E test forwarding request:");
    println!("  Forward address: {}", forward_addr);
    println!("  Destination domain: {}", dest_domain);
    println!("  Destination recipient: {}", dest_recipient);
    println!();

    println!("Backend running on http://localhost:8080");
    println!("Available endpoints:");
    println!("  GET  http://localhost:8080/forwarding-requests");
    println!("  POST http://localhost:8080/forwarding-requests");
    println!("  PATCH http://localhost:8080/forwarding-requests/{{id}}/status");
    println!();

    // Keep the backend running
    backend.serve().await
}
