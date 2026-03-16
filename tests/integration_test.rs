use forwarding_relayer::{
    derive_forwarding_address, Backend, CreateForwardingRequest, ForwardingRequest,
};

#[test]
fn test_backend_api() {
    let _ = std::fs::remove_file("storage/test_backend_3001.db");

    let backend = Backend::new(3001, "storage/test_backend_3001.db".into(), None).unwrap();
    let state = backend.state();

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

    let requests = state.list_requests().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].forward_addr, forward_addr);

    let removed = state.remove_by_addr(&forward_addr).unwrap();
    assert!(removed.is_some());

    let requests = state.list_requests().unwrap();
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

#[test]
fn test_idempotent_create() {
    let _ = std::fs::remove_file("storage/test_backend_3002.db");

    let backend = Backend::new(3002, "storage/test_backend_3002.db".into(), None).unwrap();
    let state = backend.state();

    let create_req = CreateForwardingRequest {
        forward_addr: "celestia1test1".to_string(),
        dest_domain: 42161,
        dest_recipient: "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000"
            .to_string(),
    };

    let (created, was_created) = state.create_request(create_req.clone()).unwrap();
    assert!(was_created);
    assert_eq!(created.forward_addr, "celestia1test1");

    let (returned, was_created_again) = state.create_request(create_req).unwrap();
    assert!(!was_created_again);
    assert_eq!(returned.forward_addr, "celestia1test1");

    let requests = state.list_requests().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].forward_addr, "celestia1test1");
}
