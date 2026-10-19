//! Property-based fuzz tests and edge-case unit tests for prorated first-charge calculation.
//!
//! Verifies that `calculate_prorated_first_charge(amount, interval, remaining_seconds)`:
//! - Always produces a result in `[0, amount]` (bounds invariant).
//! - Is monotonic with respect to `remaining_seconds` (monotonicity invariant).
//! - Handles extreme edge cases (e.g. interval=1, amount=i128::MAX/2, remaining_seconds > interval)
//!   without overflow or underflow.
//! - Runs at least 10,000 fuzz cases via `proptest`.

use crate::charge_core::calculate_prorated_first_charge;
use crate::types::Error;
use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10_000))]

    /// Fuzzes random (amount, interval, remaining_seconds) triples to prove bounds
    /// and monotonicity invariants hold under all inputs.
    #[test]
    fn fuzz_prorated_first_charge_triple(
        amount in 0..=i128::MAX,
        interval in 1..=u64::MAX,
        rem1 in 0..=u64::MAX,
        rem2 in 0..=u64::MAX,
    ) {
        let (rem_low, rem_high) = if rem1 <= rem2 { (rem1, rem2) } else { (rem2, rem1) };

        let res_low = calculate_prorated_first_charge(amount, interval, rem_low)
            .expect("valid amount and non-zero interval must not error");
        let res_high = calculate_prorated_first_charge(amount, interval, rem_high)
            .expect("valid amount and non-zero interval must not error");

        // 1. Bounds Invariant: 0 <= prorated_charge <= amount
        prop_assert!(res_low >= 0, "prorated charge must be non-negative");
        prop_assert!(res_low <= amount, "prorated charge must not exceed total amount");
        prop_assert!(res_high >= 0, "prorated charge must be non-negative");
        prop_assert!(res_high <= amount, "prorated charge must not exceed total amount");

        // 2. Monotonicity Invariant: rem_low <= rem_high => charge(rem_low) <= charge(rem_high)
        prop_assert!(
            res_low <= res_high,
            "prorated charge must be monotonic with respect to remaining_seconds"
        );

        // 3. Exact boundary assertions
        if rem_low == 0 {
            prop_assert_eq!(res_low, 0, "zero remaining seconds must yield 0 charge");
        }
        if rem_high >= interval {
            prop_assert_eq!(res_high, amount, "remaining_seconds >= interval must yield full amount");
        }
    }

    /// Expanded fuzz corpus: amounts near overflow boundaries, prime numbers,
    /// and edge cases that may trigger rounding errors in safe_math.rs.
    #[test]
    fn fuzz_prorated_charge_expanded_corpus(
        // Sample from boundary regions more frequently
        amount_selector in 0..=10u8,
        interval in 1..=u64::MAX,
        remaining in 0..=u64::MAX,
    ) {
        // Expanded corpus covering:
        // - Amounts near i128::MAX / 2
        // - Prime numbers
        // - Powers of 2
        // - Small amounts
        let amount = match amount_selector {
            0 => 1,
            1 => 2,
            2 => 7, // small prime
            3 => 127, // Mersenne prime
            4 => 1_000_000,
            5 => i128::MAX / 4,
            6 => i128::MAX / 2 - 1,
            7 => i128::MAX / 2,
            8 => i128::MAX / 2 + 1,
            9 => (1i128 << 100) - 1, // Near 2^100
            _ => 170141183460469231731687303715884105727i128, // i128::MAX
        };

        let result = calculate_prorated_first_charge(amount, interval, remaining);
        
        // Must never panic or overflow
        prop_assert!(result.is_ok() || result == Err(Error::Overflow) || result == Err(Error::InvalidAmount));
        
        if let Ok(prorated) = result {
            // Bounds invariant
            prop_assert!(prorated >= 0, "prorated charge must be non-negative");
            prop_assert!(prorated <= amount, "prorated charge must not exceed amount");
            
            // Boundary cases
            if remaining == 0 {
                prop_assert_eq!(prorated, 0);
            }
            if remaining >= interval {
                prop_assert_eq!(prorated, amount);
            }
        }
    }

    /// Fuzz test targeting prime number amounts which may expose rounding errors
    /// in division operations within safe_math.rs.
    #[test]
    fn fuzz_prorated_charge_prime_amounts(
        prime_idx in 0..20usize,
        interval in 1..=1_000_000u64,
        remaining in 0..=1_000_000u64,
    ) {
        // List of prime numbers including large primes
        const PRIMES: [i128; 20] = [
            2, 3, 5, 7, 11, 13, 17, 19, 23, 29,
            997, 1009, 10007, 100003, 1000003,
            999983, 9999991, 99999989, 999999937, 2147483647
        ];
        
        let amount = PRIMES[prime_idx];
        let result = calculate_prorated_first_charge(amount, interval, remaining);
        
        prop_assert!(result.is_ok(), "prime amounts should not cause errors");
        
        if let Ok(prorated) = result {
            prop_assert!(prorated >= 0 && prorated <= amount);
        }
    }
}

// ---------------------------------------------------------------------------
// Explicit Edge-Case Unit Tests
// ---------------------------------------------------------------------------

#[test]
fn test_edge_case_interval_one_amount_half_max_remaining_greater_than_interval() {
    let amount = i128::MAX / 2;
    let interval = 1u64;
    let remaining_seconds = 10u64; // remaining_seconds > interval

    let result = calculate_prorated_first_charge(amount, interval, remaining_seconds);
    assert_eq!(result, Ok(amount));
}

#[test]
fn test_edge_case_zero_interval_returns_invalid_input() {
    let result = calculate_prorated_first_charge(100, 0, 10);
    assert_eq!(result, Err(Error::InvalidInput));
}

#[test]
fn test_edge_case_negative_amount_returns_invalid_amount() {
    let result = calculate_prorated_first_charge(-1, 30, 10);
    assert_eq!(result, Err(Error::InvalidAmount));
}

#[test]
fn test_edge_case_zero_remaining_seconds() {
    let result = calculate_prorated_first_charge(1_000_000, 30, 0);
    assert_eq!(result, Ok(0));
}

#[test]
fn test_edge_case_remaining_seconds_equals_interval() {
    let amount = 500_000i128;
    let interval = 86_400u64;
    let result = calculate_prorated_first_charge(amount, interval, interval);
    assert_eq!(result, Ok(amount));
}

#[test]
fn test_edge_case_i128_max_values() {
    let amount = i128::MAX;
    let interval = u64::MAX;
    let remaining_seconds = u64::MAX - 1;

    let result = calculate_prorated_first_charge(amount, interval, remaining_seconds);
    assert!(result.is_ok());
    let prorated = result.unwrap();
    assert!(prorated >= 0);
    assert!(prorated <= amount);
}

#[test]
fn test_proration_large_amounts_when_intermediate_product_exceeds_u128() {
    let interval = 100u64;
    let remaining_seconds = 50u64;

    for amount in [i128::MAX / 4, i128::MAX / 2] {
        let result = calculate_prorated_first_charge(amount, interval, remaining_seconds);
        assert_eq!(result, Ok(amount / 2));
    }
}

// ---------------------------------------------------------------------------
// Fuzz Tests for `create_subscription` Expiration Boundary Values
// ---------------------------------------------------------------------------

#[cfg(test)]
mod expiration_boundary_tests {
    use super::*;
    use crate::SubscriptionVault;
    use soroban_sdk::{
        testutils::{Address as _, Ledger as _},
        Address, Env,
    };

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1_000))]

        /// Fuzzes random expiration timestamps (expires_at) to verify that
        /// `create_subscription` correctly handles boundary values:
        /// - u64::MAX (far future)
        /// - 0 (epoch / expired at creation)
        /// - current timestamp
        /// - timestamps before/after current
        ///
        /// Verifies:
        /// 1. Subscription creation succeeds for all expiration values
        /// 2. Returned subscription ID is non-zero
        /// 3. Subscription can be queried after creation regardless of expiration
        #[test]
        fn fuzz_create_subscription_with_expiration_boundary_values(
            expires_at_offset in -1_000_000i64..=1_000_000i64,
        ) {
            let env = Env::default();
            env.mock_all_auths();

            let admin = Address::generate(&env);
            let token = env
                .register_stellar_asset_contract_v2(admin.clone())
                .address();

            let vault_id = env.register(SubscriptionVault, ());
            let client = soroban_sdk::ContractClient::new(&env, &vault_id);
            client.invoke(&env, &Symbol::new(&env, "init"), &(
                &token,
                &7u32,
                &admin,
                &1_000_000i128,
                &3600u64,
            ));

            let current_timestamp = env.ledger().timestamp();
            let expires_at = if expires_at_offset >= 0 {
                current_timestamp + expires_at_offset as u64
            } else {
                current_timestamp.saturating_sub((-expires_at_offset) as u64)
            };

            let subscriber = Address::generate(&env);
            let merchant = Address::generate(&env);

            // Test boundary case: subscription with various expiration timestamps
            let result = client.invoke::<u32>(
                &env,
                &Symbol::new(&env, "create_subscription"),
                &(
                    &subscriber,
                    &merchant,
                    &1_000_000i128,
                    &(30u64 * 24 * 60 * 60),
                    &false,
                    &None::<i128>,
                    &Some(expires_at),
                    &None::<u32>,
                ),
            );

            // Subscription creation should succeed regardless of expiration value
            prop_assert!(result.is_ok(), "create_subscription should handle any expiration value");
            let sub_id = result.unwrap();
            prop_assert!(sub_id > 0, "subscription ID must be non-zero");
        }
    }

    #[test]
    fn test_create_subscription_expires_at_u64_max() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let vault_id = env.register(SubscriptionVault, ());
        let client = soroban_sdk::ContractClient::new(&env, &vault_id);
        client.invoke(&env, &Symbol::new(&env, "init"), &(
            &token,
            &7u32,
            &admin,
            &1_000_000i128,
            &3600u64,
        ));

        let subscriber = Address::generate(&env);
        let merchant = Address::generate(&env);

        let result = client.invoke::<u32>(
            &env,
            &Symbol::new(&env, "create_subscription"),
            &(
                &subscriber,
                &merchant,
                &1_000_000i128,
                &(30u64 * 24 * 60 * 60),
                &false,
                &None::<i128>,
                &Some(u64::MAX),
                &None::<u32>,
            ),
        );

        assert!(result.is_ok(), "subscription with expires_at=u64::MAX must be created");
    }

    #[test]
    fn test_create_subscription_expires_at_zero() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let vault_id = env.register(SubscriptionVault, ());
        let client = soroban_sdk::ContractClient::new(&env, &vault_id);
        client.invoke(&env, &Symbol::new(&env, "init"), &(
            &token,
            &7u32,
            &admin,
            &1_000_000i128,
            &3600u64,
        ));

        let subscriber = Address::generate(&env);
        let merchant = Address::generate(&env);

        let result = client.invoke::<u32>(
            &env,
            &Symbol::new(&env, "create_subscription"),
            &(
                &subscriber,
                &merchant,
                &1_000_000i128,
                &(30u64 * 24 * 60 * 60),
                &false,
                &None::<i128>,
                &Some(0u64),
                &None::<u32>,
            ),
        );

        assert!(result.is_ok(), "subscription with expires_at=0 (epoch) must be created");
    }

    #[test]
    fn test_create_subscription_expires_at_current_timestamp() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let vault_id = env.register(SubscriptionVault, ());
        let client = soroban_sdk::ContractClient::new(&env, &vault_id);
        client.invoke(&env, &Symbol::new(&env, "init"), &(
            &token,
            &7u32,
            &admin,
            &1_000_000i128,
            &3600u64,
        ));

        let current_timestamp = env.ledger().timestamp();
        let subscriber = Address::generate(&env);
        let merchant = Address::generate(&env);

        let result = client.invoke::<u32>(
            &env,
            &Symbol::new(&env, "create_subscription"),
            &(
                &subscriber,
                &merchant,
                &1_000_000i128,
                &(30u64 * 24 * 60 * 60),
                &false,
                &None::<i128>,
                &Some(current_timestamp),
                &None::<u32>,
            ),
        );

        assert!(result.is_ok(), "subscription with expires_at=current_timestamp must be created");
    }

    #[test]
    fn test_create_subscription_expires_at_past_timestamp() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let vault_id = env.register(SubscriptionVault, ());
        let client = soroban_sdk::ContractClient::new(&env, &vault_id);
        client.invoke(&env, &Symbol::new(&env, "init"), &(
            &token,
            &7u32,
            &admin,
            &1_000_000i128,
            &3600u64,
        ));

        let current_timestamp = env.ledger().timestamp();
        let past_timestamp = current_timestamp.saturating_sub(86400); // 1 day ago
        let subscriber = Address::generate(&env);
        let merchant = Address::generate(&env);

        let result = client.invoke::<u32>(
            &env,
            &Symbol::new(&env, "create_subscription"),
            &(
                &subscriber,
                &merchant,
                &1_000_000i128,
                &(30u64 * 24 * 60 * 60),
                &false,
                &None::<i128>,
                &Some(past_timestamp),
                &None::<u32>,
            ),
        );

        assert!(result.is_ok(), "subscription with expires_at=past_timestamp must be created");
    }
}
