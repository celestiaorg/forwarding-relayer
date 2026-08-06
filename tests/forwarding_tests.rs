use chrono::{Duration as ChronoDuration, Utc};
use forwarding_relayer::{
    balances_equal, derive_forwarding_address, retirement_reason, retry_delay, Balance,
    RetireReason,
};
use std::time::Duration;

const ONE_DAY: u64 = 86_400;
const SEVEN_DAYS: u64 = 604_800;

#[test]
fn test_retirement_never_active_within_age_kept() {
    // A never-funded address younger than max_request_age_seconds is kept.
    let now = Utc::now();
    let created_at = (now - ChronoDuration::seconds(3600)).to_rfc3339();
    assert_eq!(
        retirement_reason(&created_at, None, now, ONE_DAY, SEVEN_DAYS),
        None
    );
}

#[test]
fn test_retirement_never_active_past_age_unfunded() {
    // A never-funded address older than max_request_age_seconds is retired as unfunded.
    let now = Utc::now();
    let created_at = (now - ChronoDuration::seconds(ONE_DAY as i64 + 60)).to_rfc3339();
    let reason = retirement_reason(&created_at, None, now, ONE_DAY, SEVEN_DAYS);
    assert!(matches!(reason, Some(RetireReason::Unfunded { .. })));
    assert_eq!(reason.unwrap().label(), "unfunded");
}

#[test]
fn test_retirement_active_recently_kept() {
    // An active address that saw activity recently is kept, even if it is old
    // (the inactivity timer, not the 1-day age, governs active addresses).
    let now = Utc::now();
    let created_at = (now - ChronoDuration::days(30)).to_rfc3339();
    let last_activity = Some(now - ChronoDuration::seconds(3600));
    assert_eq!(
        retirement_reason(&created_at, last_activity, now, ONE_DAY, SEVEN_DAYS),
        None
    );
}

#[test]
fn test_retirement_active_idle_past_inactivity() {
    // An active address idle past max_address_inactivity_seconds is retired as inactive.
    let now = Utc::now();
    let created_at = (now - ChronoDuration::days(30)).to_rfc3339();
    let last_activity = Some(now - ChronoDuration::seconds(SEVEN_DAYS as i64 + 60));
    let reason = retirement_reason(&created_at, last_activity, now, ONE_DAY, SEVEN_DAYS);
    assert!(matches!(reason, Some(RetireReason::Inactive { .. })));
    assert_eq!(reason.unwrap().label(), "inactive");
}

#[test]
fn test_retry_delay_schedule() {
    // Backoff grows 30s -> 1m -> 5m -> 30m -> 60m as failures accumulate.
    assert_eq!(retry_delay(1), Duration::from_secs(30));
    assert_eq!(retry_delay(2), Duration::from_secs(60));
    assert_eq!(retry_delay(3), Duration::from_secs(300));
    assert_eq!(retry_delay(4), Duration::from_secs(1800));
    assert_eq!(retry_delay(5), Duration::from_secs(3600));
}

#[test]
fn test_retry_delay_saturates_at_one_hour() {
    // Beyond the schedule, every further attempt waits the 1-hour cap,
    // regardless of how high the failure count climbs.
    assert_eq!(retry_delay(6), Duration::from_secs(3600));
    assert_eq!(retry_delay(100), Duration::from_secs(3600));
    assert_eq!(retry_delay(u32::MAX), Duration::from_secs(3600));
}

#[test]
fn test_retry_delay_zero_failures_defensive() {
    // failures should always be >= 1 in practice; guard against underflow.
    assert_eq!(retry_delay(0), Duration::from_secs(30));
}

#[test]
fn test_derive_forwarding_address_default() {
    // Test case for the default parameters used in make transfer
    let dest_domain = 1234;
    let dest_recipient = "0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d";
    let token_id = "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131";

    let result = derive_forwarding_address(dest_domain, dest_recipient, token_id, None, None);
    assert!(result.is_ok());

    let address = result.unwrap();
    assert!(address.starts_with("celestia1"));
    println!("Derived address (domain 1234): {}", address);
}

#[test]
fn test_derive_forwarding_address_different_token_ids() {
    // Different token_ids for the same domain/recipient should produce different addresses
    let dest_domain = 1234;
    let dest_recipient = "0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d";
    let token_id_a = "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131";
    let token_id_b = "0x0000000000000000000000001234567890abcdef1234567890abcdef12345678";

    let address_a =
        derive_forwarding_address(dest_domain, dest_recipient, token_id_a, None, None).unwrap();
    let address_b =
        derive_forwarding_address(dest_domain, dest_recipient, token_id_b, None, None).unwrap();

    assert_ne!(
        address_a, address_b,
        "Different token_ids must produce different forwarding addresses"
    );
}

#[test]
fn test_derive_forwarding_address_test_vectors() {
    // Cross-platform test vectors from celestia-app PR #6906
    // These verify the Rust derivation matches the Go implementation exactly.
    let test_vectors = vec![
        (
            "vector_1_ethereum_mainnet",
            1u32,
            "000000000000000000000000deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
            "726f757465725f61707000000000000000000000000000010000000000000000",
            "celestia1cg34qulzr4m78vwvg56c5ftn69frhulamgy8qe",
        ),
        (
            "vector_2_arbitrum",
            42161,
            "0000000000000000000000001234567890abcdef1234567890abcdef12345678",
            "726f757465725f61707000000000000000000000000000010000000000000001",
            "celestia1x8dplhx74cdnguq3sxdhgmw8mp30s3z57qnade",
        ),
        (
            "vector_3_zero_values",
            0,
            "0000000000000000000000000000000000000000000000000000000000000000",
            "726f757465725f61707000000000000000000000000000010000000000000002",
            "celestia1lezkhrla6g2h3403n45d6czr7gfqahe8hhj8p8",
        ),
    ];

    for (name, dest_domain, dest_recipient, token_id, expected_bech32) in test_vectors {
        let address = derive_forwarding_address(
            dest_domain,
            &format!("0x{}", dest_recipient),
            &format!("0x{}", token_id),
            None,
            None,
        )
        .unwrap_or_else(|e| panic!("{}: derivation failed: {}", name, e));
        assert_eq!(address, expected_bech32, "test vector {} failed", name);
    }
}

#[test]
fn test_balances_equal() {
    let balances1 = vec![
        Balance {
            denom: "utia".to_string(),
            amount: "1000".to_string(),
        },
        Balance {
            denom: "uatom".to_string(),
            amount: "500".to_string(),
        },
    ];

    let balances2 = vec![
        Balance {
            denom: "uatom".to_string(),
            amount: "500".to_string(),
        },
        Balance {
            denom: "utia".to_string(),
            amount: "1000".to_string(),
        },
    ];

    assert!(balances_equal(&balances1, &balances2));

    let balances3 = vec![Balance {
        denom: "utia".to_string(),
        amount: "2000".to_string(),
    }];

    assert!(!balances_equal(&balances1, &balances3));
}

/// Cross-implementation vectors produced by celestia-app's
/// x/forwarding/types.DeriveForwardingAddress. If the two derivations diverge, deposits go to
/// an address the chain will not forward, so these must stay exact.
#[test]
fn test_derive_forwarding_address_hook_matches_go() {
    use bech32::{Bech32, Hrp};

    const DEST_DOMAIN: u32 = 42161;
    const DEST_RECIPIENT: &str =
        "0x00000000000000000000000042d35cc6634c0532925a3b844bc9e7595f0beb00";
    const TOKEN_ID: &str = "0x726f757465725f61707000000000000000000000000000010000000000000000";
    const HOOK_ID: &str = "0x726f757465725f706f73745f6469737061746368000000040000000000000009";
    const METADATA: &str = "0xabcdef";

    let hrp = Hrp::parse("celestia").unwrap();
    let bech = |h: &str| bech32::encode::<Bech32>(hrp, &hex::decode(h).unwrap()).unwrap();

    // (hook, metadata, expected Go output)
    let cases: [(Option<&str>, Option<&str>, &str); 4] = [
        (
            Some(HOOK_ID),
            None,
            "6f1b41767279393fd1fbec23f090fa84db7e2d28",
        ),
        (
            Some(HOOK_ID),
            Some(METADATA),
            "15b80919673356e9cf6c56ab336f857635b7c29b",
        ),
        (
            None,
            Some(METADATA),
            "d5f42de3575ee4359ffd0ef162abef858f91469f",
        ),
        (None, None, "27e0c578f2c44de8ddd0bb58782069b65054b987"),
    ];

    for (hook, metadata, expected_hex) in cases {
        let got = forwarding_relayer::derive_forwarding_address(
            DEST_DOMAIN,
            DEST_RECIPIENT,
            TOKEN_ID,
            hook,
            metadata,
        )
        .unwrap();
        assert_eq!(
            got,
            bech(expected_hex),
            "derivation must match Go for hook={hook:?} metadata={metadata:?}"
        );
    }

    // The no-binding case must also equal the plain v1 helper.
    assert_eq!(
        derive_forwarding_address(DEST_DOMAIN, DEST_RECIPIENT, TOKEN_ID, None, None).unwrap(),
        bech("27e0c578f2c44de8ddd0bb58782069b65054b987")
    );
}

/// None, empty, and the zero address all mean "mailbox default hook". With no metadata that
/// must collapse to the version-1 address, matching the chain's normalisation.
#[test]
fn test_derive_forwarding_address_zero_hook_is_default() {
    const DEST_DOMAIN: u32 = 42161;
    const DEST_RECIPIENT: &str =
        "0x00000000000000000000000042d35cc6634c0532925a3b844bc9e7595f0beb00";
    const TOKEN_ID: &str = "0x726f757465725f61707000000000000000000000000000010000000000000000";
    const ZERO: &str = "0x0000000000000000000000000000000000000000000000000000000000000000";

    let base =
        derive_forwarding_address(DEST_DOMAIN, DEST_RECIPIENT, TOKEN_ID, None, None).unwrap();

    for hook in [None, Some(""), Some(ZERO)] {
        let got = forwarding_relayer::derive_forwarding_address(
            DEST_DOMAIN,
            DEST_RECIPIENT,
            TOKEN_ID,
            hook,
            None,
        )
        .unwrap();
        assert_eq!(
            got, base,
            "hook {hook:?} with no metadata must be the default"
        );

        // ...but the same hook paired with metadata is a binding, not the default.
        let bound = forwarding_relayer::derive_forwarding_address(
            DEST_DOMAIN,
            DEST_RECIPIENT,
            TOKEN_ID,
            hook,
            Some("0xabcdef"),
        )
        .unwrap();
        assert_ne!(
            bound, base,
            "metadata alone must still bind for hook {hook:?}"
        );
    }
}

#[test]
fn test_derive_forwarding_address_rejects_bad_hook_length() {
    let err = forwarding_relayer::derive_forwarding_address(
        1,
        "0x0000000000000000000000000000000000000000000000000000000000000001",
        "0x0000000000000000000000000000000000000000000000000000000000000002",
        Some("0xdeadbeef"),
        None,
    )
    .unwrap_err();
    assert!(err.to_string().contains("32 bytes"), "got: {err}");
}
