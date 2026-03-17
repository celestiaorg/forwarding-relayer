use clap::Parser;
use forwarding_relayer::{
    derive_forwarding_address, init_metrics_exporter, oldest_pending_request_age_seconds,
    parse_metric_amount, Backend, Cli, Command, ForwardingRequest,
};
use reqwest::Client;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::time::sleep;

fn env_lock() -> &'static Mutex<()> {
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    ENV_LOCK.get_or_init(|| Mutex::new(()))
}

fn http_client() -> Client {
    Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn test_backend_api() {
    // Clean up any existing test database
    let _ = std::fs::remove_file("storage/test_backend_3001.db");

    // Start backend
    let backend = Backend::new(3001, "storage/test_backend_3001.db".into(), false).unwrap();
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

    let client = http_client();
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
        .delete(format!("{}/forwarding-requests/{}", base_url, forward_addr))
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

#[tokio::test]
async fn test_idempotent_create() {
    use forwarding_relayer::CreateForwardingRequest;

    // Clean up any existing test database
    let _ = std::fs::remove_file("storage/test_backend_3002.db");

    // Start backend
    let backend = Backend::new(3002, "storage/test_backend_3002.db".into(), false).unwrap();

    // Start the server in the background
    tokio::spawn(async move {
        backend.serve().await.ok();
    });

    // Give the server time to start
    sleep(Duration::from_millis(100)).await;

    let client = http_client();
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

#[test]
fn test_backend_metrics_port_cli_parse() {
    let cli = Cli::parse_from([
        "forwarding-relayer",
        "backend",
        "--port",
        "8080",
        "--metrics-port",
        "9091",
    ]);

    match cli.command {
        Command::Backend(config) => {
            assert_eq!(config.port, 8080);
            assert_eq!(config.metrics_port, Some(9091));
        }
        _ => panic!("expected backend command"),
    }
}

#[test]
fn test_relayer_metrics_port_env_parse() {
    let _guard = env_lock().lock().unwrap();

    std::env::set_var("PRIVATE_KEY_HEX", "abcd");
    std::env::set_var("RELAYER_METRICS_PORT", "9191");

    let cli = Cli::parse_from(["forwarding-relayer", "relayer"]);

    std::env::remove_var("PRIVATE_KEY_HEX");
    std::env::remove_var("RELAYER_METRICS_PORT");

    match cli.command {
        Command::Relayer(config) => {
            assert_eq!(config.metrics_port, Some(9191));
            assert_eq!(config.private_key_hex, "abcd");
        }
        _ => panic!("expected relayer command"),
    }
}

#[test]
fn test_backend_metrics_port_env_parse() {
    let _guard = env_lock().lock().unwrap();

    std::env::set_var("BACKEND_METRICS_PORT", "9292");

    let cli = Cli::parse_from(["forwarding-relayer", "backend"]);

    std::env::remove_var("BACKEND_METRICS_PORT");

    match cli.command {
        Command::Backend(config) => {
            assert_eq!(config.metrics_port, Some(9292));
        }
        _ => panic!("expected backend command"),
    }
}

#[test]
fn test_metric_helpers() {
    assert_eq!(parse_metric_amount("1234"), Some(1234.0));
    assert!(parse_metric_amount("not-a-number").is_none());
    assert_eq!(oldest_pending_request_age_seconds(None).unwrap(), 0.0);
}

#[tokio::test]
async fn test_backend_metrics_endpoint() {
    let _ = std::fs::remove_file("storage/test_backend_3003.db");
    let _ = std::fs::remove_file("storage/test_backend_3004.db");

    let disabled_backend =
        Backend::new(3003, "storage/test_backend_3003.db".into(), false).unwrap();
    tokio::spawn(async move {
        disabled_backend.serve().await.ok();
    });
    wait_for_status("http://127.0.0.1:3003/forwarding-requests", 200).await;

    let client = http_client();
    let disabled_metrics = client.get("http://127.0.0.1:4011/metrics").send().await;
    assert!(disabled_metrics.is_err());

    init_metrics_exporter(Some(4012)).unwrap();

    let enabled_backend = Backend::new(3004, "storage/test_backend_3004.db".into(), true).unwrap();
    tokio::spawn(async move {
        enabled_backend.serve().await.ok();
    });

    wait_for_status("http://127.0.0.1:3004/forwarding-requests", 200).await;
    wait_for_status("http://127.0.0.1:4012/metrics", 200).await;

    let create_req = forwarding_relayer::CreateForwardingRequest {
        forward_addr: "celestia1metrics".to_string(),
        dest_domain: 1234,
        dest_recipient: "0x000000000000000000000000f39Fd6e51aad88F6F4ce6aB8827279cffFb92266"
            .to_string(),
    };

    let response = client
        .post("http://127.0.0.1:3004/forwarding-requests")
        .json(&create_req)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    let metrics_after_create = client
        .get("http://127.0.0.1:4012/metrics")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_metric_value(&metrics_after_create, "pending_requests", 1.0);
    assert!(metrics_after_create.contains("requests_created_total{result=\"created\"} 1"));
    assert!(metrics_after_create.contains("oldest_pending_request_age_seconds"));

    let response = client
        .delete("http://127.0.0.1:3004/forwarding-requests/celestia1metrics")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    let metrics_after_delete = client
        .get("http://127.0.0.1:4012/metrics")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_metric_value(&metrics_after_delete, "pending_requests", 0.0);
    assert!(metrics_after_delete.contains("requests_completed_total{result=\"removed\"} 1"));
    assert_metric_non_negative(&metrics_after_delete, "oldest_pending_request_age_seconds");
}

async fn wait_for_status(url: &str, expected_status: u16) {
    let client = http_client();

    for _ in 0..40 {
        if let Ok(response) = client.get(url).send().await {
            if response.status().as_u16() == expected_status {
                return;
            }
        }
        sleep(Duration::from_millis(50)).await;
    }

    panic!("timed out waiting for {url}");
}

fn assert_metric_non_negative(metrics: &str, metric_name: &str) {
    let parsed = metric_value(metrics, metric_name);
    assert!(parsed >= 0.0, "metric {metric_name} must be non-negative");
}

fn assert_metric_value(metrics: &str, metric_name: &str, expected: f64) {
    let parsed = metric_value(metrics, metric_name);
    assert_eq!(parsed, expected, "unexpected value for {metric_name}");
}

fn metric_value(metrics: &str, metric_name: &str) -> f64 {
    let line = metrics
        .lines()
        .find(|line| line.starts_with(metric_name))
        .unwrap_or_else(|| panic!("missing metric {metric_name}"));
    let value = line
        .split_whitespace()
        .last()
        .unwrap_or_else(|| panic!("invalid metric line for {metric_name}"));
    value.parse::<f64>().unwrap()
}
