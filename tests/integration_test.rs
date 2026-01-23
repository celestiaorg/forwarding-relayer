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
        id: "test-request-1".to_string(),
        forward_addr: forward_addr.clone(),
        dest_domain,
        dest_recipient: dest_recipient.to_string(),
        status: "pending".to_string(),
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
    assert_eq!(requests[0].status, "pending");

    // Test PATCH /forwarding-requests/{id}/status
    let update = serde_json::json!({
        "status": "completed"
    });

    let response = client
        .patch(format!(
            "{}/forwarding-requests/test-request-1/status",
            base_url
        ))
        .json(&update)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);

    // Verify the request was removed from storage (completed requests are deleted)
    let response = client
        .get(format!("{}/forwarding-requests", base_url))
        .send()
        .await
        .unwrap();

    let requests: Vec<ForwardingRequest> = response.json().await.unwrap();
    assert_eq!(requests.len(), 0); // Request should be removed after completion
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
async fn test_auto_generated_ids() {
    use forwarding_relayer::{Backend, CreateForwardingRequest};

    // Clean up any existing test database
    let _ = std::fs::remove_file("storage/test_backend_3002.db");

    // Start backend
    let backend = Backend::new(3002, "storage/test_backend_3002.db".into()).unwrap();
    let _state = backend.state();

    // Start the server in the background
    tokio::spawn(async move {
        backend.serve().await.ok();
    });

    // Give the server time to start
    sleep(Duration::from_millis(100)).await;

    let client = Client::new();
    let base_url = "http://127.0.0.1:3002";

    // Create first request via POST (no ID provided)
    let create_req = CreateForwardingRequest {
        forward_addr: "celestia1test1".to_string(),
        dest_domain: 42161,
        dest_recipient: "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000"
            .to_string(),
    };

    let response = client
        .post(format!("{}/forwarding-requests", base_url))
        .json(&create_req)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 201); // Created

    let created1: ForwardingRequest = response.json().await.unwrap();
    assert_eq!(created1.id, "req-000001"); // First auto-generated ID
    assert_eq!(created1.forward_addr, "celestia1test1");
    assert_eq!(created1.status, "pending");

    // Create second request - should get next ID
    let create_req2 = CreateForwardingRequest {
        forward_addr: "celestia1test1".to_string(), // Same address!
        dest_domain: 10,
        dest_recipient: "0x000000000000000000000000A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
            .to_string(),
    };

    let response2 = client
        .post(format!("{}/forwarding-requests", base_url))
        .json(&create_req2)
        .send()
        .await
        .unwrap();

    let created2: ForwardingRequest = response2.json().await.unwrap();
    assert_eq!(created2.id, "req-000002"); // Second auto-generated ID
    assert_eq!(created2.forward_addr, "celestia1test1"); // Same address as first!

    // List all requests - should have both
    let response = client
        .get(format!("{}/forwarding-requests", base_url))
        .send()
        .await
        .unwrap();

    let requests: Vec<ForwardingRequest> = response.json().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].id, "req-000001");
    assert_eq!(requests[1].id, "req-000002");

    // Both have the same address but different IDs - no collision!
    assert_eq!(requests[0].forward_addr, requests[1].forward_addr);
}
