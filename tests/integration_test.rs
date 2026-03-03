use forwarding_relayer::{derive_forwarding_address, Backend, ForwardingRequest};
use reqwest::Client;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn test_backend_api() {
    // Clean up any existing test database
    let _ = std::fs::remove_file("storage/test_backend_3001.db");

    // Start backend
    let backend = Backend::new(3001, "storage/test_backend_3001.db".into()).unwrap();
    let state = backend.state();

    // Add a test request
    let dest_domain = 42161;
    let dest_recipient = "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000";
    let forward_addr = derive_forwarding_address(dest_domain, dest_recipient).unwrap();

    let request = ForwardingRequest {
        forward_addr: forward_addr.clone(),
        dest_domain,
        dest_recipient: dest_recipient.to_string(),
        created_at: None,
    };

    state.add_request(request).unwrap();

    // Start the server in the background
    tokio::spawn(async move {
        backend.serve().await.ok();
    });

    // Give the server time to start
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let base_url = "http://127.0.0.1:3001";

    // Test GET /forwarding-requests
    let response = client
        .get(format!("{}/forwarding-requests", base_url))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    let requests: Vec<ForwardingRequest> = response.json().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].forward_addr, forward_addr);

    // Test DELETE /forwarding-requests/{addr} - mark as completed
    let response = client
        .delete(format!(
            "{}/forwarding-requests/{}",
            base_url, forward_addr
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Verify the request was removed from storage
    let response = client
        .get(format!("{}/forwarding-requests", base_url))
        .send()
        .await
        .unwrap();

    let requests: Vec<ForwardingRequest> = response.json().await.unwrap();
    assert_eq!(requests.len(), 0);
}

#[test]
fn test_derive_forwarding_address() {
    // Test address derivation
    let dest_domain = 42161;
    let dest_recipient = "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000";

    let address = derive_forwarding_address(dest_domain, dest_recipient).unwrap();

    // Should be a valid bech32 address with celestia prefix
    assert!(address.starts_with("celestia1"));
    assert!(address.len() > 20);
}

#[test]
fn test_balance_cache_serialization() {
    use forwarding_relayer::Balance;
    use std::collections::HashMap;

    let mut cache: HashMap<String, Vec<Balance>> = HashMap::new();

    cache.insert(
        "celestia1test".to_string(),
        vec![Balance {
            denom: "utia".to_string(),
            amount: "1000000".to_string(),
        }],
    );

    // Test serialization
    let json = serde_json::to_string(&cache).unwrap();
    assert!(json.contains("celestia1test"));
    assert!(json.contains("utia"));

    // Test deserialization
    let deserialized: HashMap<String, Vec<Balance>> = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.len(), 1);
    assert_eq!(deserialized["celestia1test"][0].denom, "utia");
    assert_eq!(deserialized["celestia1test"][0].amount, "1000000");
}

#[tokio::test]
async fn test_idempotent_create() {
    use forwarding_relayer::CreateForwardingRequest;

    // Clean up any existing test database
    let _ = std::fs::remove_file("storage/test_backend_3002.db");

    // Start backend
    let backend = Backend::new(3002, "storage/test_backend_3002.db".into()).unwrap();

    // Start the server in the background
    tokio::spawn(async move {
        backend.serve().await.ok();
    });

    // Give the server time to start
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let base_url = "http://127.0.0.1:3002";

    let create_req = CreateForwardingRequest {
        forward_addr: "celestia1test1".to_string(),
        dest_domain: 42161,
        dest_recipient: "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000"
            .to_string(),
    };

    // First POST - should create
    let response = client
        .post(format!("{}/forwarding-requests", base_url))
        .json(&create_req)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 201); // Created
    let created: ForwardingRequest = response.json().await.unwrap();
    assert_eq!(created.forward_addr, "celestia1test1");

    // Second POST for the same address - should return existing
    let response2 = client
        .post(format!("{}/forwarding-requests", base_url))
        .json(&create_req)
        .send()
        .await
        .unwrap();

    assert_eq!(response2.status(), 200); // OK - returned existing, not 201 Created
    let returned: ForwardingRequest = response2.json().await.unwrap();
    assert_eq!(returned.forward_addr, "celestia1test1");

    // List all requests - should only have one
    let response = client
        .get(format!("{}/forwarding-requests", base_url))
        .send()
        .await
        .unwrap();

    let requests: Vec<ForwardingRequest> = response.json().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].forward_addr, "celestia1test1");
}
