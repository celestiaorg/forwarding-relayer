use forwarding_relayer::{balances_equal, derive_forwarding_address, Balance};

#[test]
fn test_derive_forwarding_address_default() {
    // Test case for the default parameters used in make transfer
    let dest_domain = 1234;
    let dest_recipient = "0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d";
    let token_id = "0x00000000000000000000000031b5234A896FbC4b3e2F7237592D054716762131";

    let result = derive_forwarding_address(dest_domain, dest_recipient, token_id);
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

    let address_a = derive_forwarding_address(dest_domain, dest_recipient, token_id_a).unwrap();
    let address_b = derive_forwarding_address(dest_domain, dest_recipient, token_id_b).unwrap();

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
