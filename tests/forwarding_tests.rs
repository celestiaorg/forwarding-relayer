use forwarding_relayer::{derive_forwarding_address, Balance, balances_equal};

#[test]
fn test_derive_forwarding_address() {
    // Test case from the spec
    let dest_domain = 42161; // Arbitrum
    let dest_recipient = "0x000000000000000000000000742d35Cc6634C0532925a3b844Bc9e7595f00000";

    let result = derive_forwarding_address(dest_domain, dest_recipient);
    assert!(result.is_ok());

    let address = result.unwrap();
    assert!(address.starts_with("celestia1"));
    println!("Derived address (domain 42161): {}", address);
}

#[test]
fn test_derive_forwarding_address_default() {
    // Test case for the default parameters used in make transfer
    let dest_domain = 1234;
    let dest_recipient = "0x000000000000000000000000aF9053bB6c4346381C77C2FeD279B17ABAfCDf4d";

    let result = derive_forwarding_address(dest_domain, dest_recipient);
    assert!(result.is_ok());

    let address = result.unwrap();
    assert!(address.starts_with("celestia1"));
    println!("Derived address (domain 1234): {}", address);
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
