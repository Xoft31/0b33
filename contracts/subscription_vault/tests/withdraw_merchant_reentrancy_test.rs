#![cfg(test)]

extern crate alloc;

use soroban_sdk::{
    contract, contractimpl,
    testutils::Address as _,
    Address, Env, String, Symbol,
};
use subscription_vault::{
    DataKey, Error, SubscriptionVault, SubscriptionVaultClient, DEFAULT_ALLOWED_OPS,
};

/// Mock malicious token that attempts a re-entry callback during the final transfer step
/// of `withdraw_merchant_funds`.
///
/// Under normal operation, `withdraw_merchant_funds` follows the CEI pattern and acquires
/// a persistent-storage `ReentrancyGuard("withdraw_merchant_funds")`. When `token.transfer()`
/// is invoked as the final interaction step, this mock contract attempts to call back into
/// `withdraw_merchant_funds` to verify that the guard intercepts the nested call and returns
/// `Error::Reentrancy (4010)`.
#[contract]
pub struct MockReentrantToken;

#[contractimpl]
impl MockReentrantToken {
    /// Initialize the mock token with vault address reference and default configuration.
    pub fn init(env: Env, vault: Address) {
        env.storage().instance().set(&Symbol::new(&env, "vault"), &vault);
        env.storage().instance().set(&Symbol::new(&env, "reenter"), &true);
        env.storage().instance().set(&Symbol::new(&env, "mode"), &0u32);
        env.storage().instance().set(&Symbol::new(&env, "callback_executed"), &false);
        env.storage().instance().set(&Symbol::new(&env, "guard_caught_reentrancy"), &false);
        env.storage().instance().set(&Symbol::new(&env, "reentry_error_code"), &0u32);
    }

    /// Set mode: 0 for withdraw_merchant_funds, 1 for withdraw_merchant_token_funds.
    pub fn set_mode(env: Env, mode: u32) {
        env.storage().instance().set(&Symbol::new(&env, "mode"), &mode);
    }

    /// Enable or disable the reentrancy callback.
    pub fn set_reenter(env: Env, reenter: bool) {
        env.storage().instance().set(&Symbol::new(&env, "reenter"), &reenter);
    }

    /// Set mock token balance for an address to satisfy vault balance checks.
    pub fn set_balance(env: Env, account: Address, amount: i128) {
        let key = Symbol::new(&env, "bal");
        env.storage().persistent().set(&(key, account), &amount);
    }

    /// SEP-41 balance function.
    pub fn balance(env: Env, id: Address) -> i128 {
        let key = Symbol::new(&env, "bal");
        env.storage().persistent().get(&(key, id)).unwrap_or(0i128)
    }

    /// SEP-41 token transfer function.
    ///
    /// When `withdraw_merchant_funds` reaches its final step:
    /// `token_client.transfer(&contract, &merchant, &amount);`
    /// this callback executes. The malicious token calls back into the vault
    /// attempting to double-withdraw while the first execution is still active.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();

        // 1. Normal transfer accounting inside the token
        let bal_key = Symbol::new(&env, "bal");
        let from_bal: i128 = env.storage().persistent().get(&(bal_key.clone(), from.clone())).unwrap_or(0i128);
        let to_bal: i128 = env.storage().persistent().get(&(bal_key.clone(), to.clone())).unwrap_or(0i128);
        env.storage().persistent().set(&(bal_key.clone(), from.clone()), &(from_bal - amount));
        env.storage().persistent().set(&(bal_key, to.clone()), &(to_bal + amount));

        // 2. Check if re-entry is enabled
        let should_reenter: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "reenter"))
            .unwrap_or(false);

        let in_reentry: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "in_reentry"))
            .unwrap_or(false);

        if should_reenter && !in_reentry {
            // Guard against infinite loops if the contract reentrancy guard were missing
            env.storage().instance().set(&Symbol::new(&env, "in_reentry"), &true);
            env.storage().instance().set(&Symbol::new(&env, "callback_executed"), &true);

            let vault_addr: Address = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "vault"))
                .unwrap_or(from.clone());

            let mode: u32 = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "mode"))
                .unwrap_or(0u32);

            let vault_client = SubscriptionVaultClient::new(&env, &vault_addr);

            // Re-entry attack: attempt recursive call during transfer callback
            let reenter_result = if mode == 0 {
                // Call withdraw_merchant_funds recursively
                vault_client.try_withdraw_merchant_funds(&to, &amount)
            } else {
                // Call withdraw_merchant_token_funds recursively
                vault_client.try_withdraw_merchant_token_funds(
                    &to,
                    &env.current_contract_address(),
                    &amount,
                )
            };

            // Inspect and record the outcome of the re-entrant call
            match reenter_result {
                Err(Ok(Error::Reentrancy)) => {
                    // Confirmed: Reentrancy guard detected recursion and returned Error::Reentrancy (4010)
                    env.storage().instance().set(&Symbol::new(&env, "guard_caught_reentrancy"), &true);
                    env.storage().instance().set(&Symbol::new(&env, "reentry_error_code"), &4010u32);
                }
                Ok(_) => {
                    // Exploit succeeded - guard failed to block re-entry
                    env.storage().instance().set(&Symbol::new(&env, "guard_caught_reentrancy"), &false);
                    env.storage().instance().set(&Symbol::new(&env, "reentry_error_code"), &0u32);
                }
                Err(Ok(err)) => {
                    // Other contract error
                    env.storage().instance().set(&Symbol::new(&env, "guard_caught_reentrancy"), &false);
                    env.storage().instance().set(&Symbol::new(&env, "reentry_error_code"), &(err as u32));
                }
                Err(Err(_)) => {
                    // Host/invoke error
                    env.storage().instance().set(&Symbol::new(&env, "guard_caught_reentrancy"), &false);
                    env.storage().instance().set(&Symbol::new(&env, "reentry_error_code"), &9999u32);
                }
            }

            env.storage().instance().set(&Symbol::new(&env, "in_reentry"), &false);
        }
    }

    /// Query whether the callback was executed during transfer.
    pub fn callback_executed(env: Env) -> bool {
        env.storage().instance().get(&Symbol::new(&env, "callback_executed")).unwrap_or(false)
    }

    /// Query whether the reentrancy guard intercepted the recursive call.
    pub fn guard_caught_reentrancy(env: Env) -> bool {
        env.storage().instance().get(&Symbol::new(&env, "guard_caught_reentrancy")).unwrap_or(false)
    }

    /// Query the numeric error code returned by the re-entrant call (expected: 4010).
    pub fn reentry_error_code(env: Env) -> u32 {
        env.storage().instance().get(&Symbol::new(&env, "reentry_error_code")).unwrap_or(0u32)
    }
}

/// Helper to set up test environment with SubscriptionVault and MockReentrantToken.
fn setup_reentrancy_test() -> (
    Env,
    SubscriptionVaultClient<'static>,
    Address,
    MockReentrantTokenClient<'static>,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);

    // Register SubscriptionVault contract
    let vault_id = env.register(SubscriptionVault, ());
    let vault_client = SubscriptionVaultClient::new(&env, &vault_id);

    // Register MockReentrantToken contract
    let token_id = env.register(MockReentrantToken, ());
    let mock_token_client = MockReentrantTokenClient::new(&env, &token_id);
    mock_token_client.init(&vault_id);

    let min_topup: i128 = 1_000_000;
    let grace_period: u64 = 7 * 24 * 60 * 60;

    // Initialize vault with mock token as the accepted token
    vault_client.init(&token_id, &6u32, &admin, &min_topup, &grace_period);

    // Initialize merchant configuration
    let redirect_url = String::from_str(&env, "https://example.com");
    vault_client.initialize_merchant_config(
        &merchant,
        &merchant,
        &0,
        &DEFAULT_ALLOWED_OPS,
        &None,
        &redirect_url,
    );

    (env, vault_client, vault_id, mock_token_client, token_id, admin, merchant)
}

/// Helper to seed merchant balance directly in vault storage.
fn seed_merchant_balance(
    env: &Env,
    vault_id: &Address,
    merchant: &Address,
    token_id: &Address,
    balance: i128,
) {
    env.as_contract(vault_id, || {
        env.storage().instance().set(
            &DataKey::MerchantBalance(merchant.clone(), token_id.clone()),
            &balance,
        );
    });
}

// ── Test 1: withdraw_merchant_funds Reentrancy Guard Protection ────────────────

/// Confirms that `withdraw_merchant_funds` acquires a reentrancy guard before
/// transferring tokens, and that a malicious token attempting a callback re-entry
/// is blocked with `Error::Reentrancy` (code 4010).
#[test]
fn test_withdraw_merchant_funds_reentrancy_guard_blocks_malicious_token_callback() {
    let (env, vault_client, vault_id, mock_token_client, token_id, _admin, merchant) =
        setup_reentrancy_test();

    let initial_balance: i128 = 100_000_000;
    let withdraw_amount: i128 = 40_000_000;

    // Seed merchant earnings in vault storage and fund the vault with mock tokens
    seed_merchant_balance(&env, &vault_id, &merchant, &token_id, initial_balance);
    mock_token_client.set_balance(&vault_id, &initial_balance);

    // Verify initial preconditions
    assert_eq!(vault_client.get_merchant_balance(&merchant), initial_balance);
    assert_eq!(mock_token_client.balance(&vault_id), initial_balance);
    assert_eq!(mock_token_client.balance(&merchant), 0);

    // Configure mode 0: re-enter `withdraw_merchant_funds`
    mock_token_client.set_mode(&0u32);
    mock_token_client.set_reenter(&true);

    // Trigger merchant withdrawal — this calls token.transfer(), which calls back into vault
    let result = vault_client.try_withdraw_merchant_funds(&merchant, &withdraw_amount);

    // 1. The outer withdrawal transaction must succeed
    assert!(
        result.is_ok(),
        "Outer withdrawal must succeed while properly blocking recursive re-entry"
    );

    // 2. Confirm the malicious token's transfer callback actually executed
    assert!(
        mock_token_client.callback_executed(),
        "Mock token transfer callback must have executed during withdrawal"
    );

    // 3. Confirm the reentrancy guard intercepted the recursive call
    assert!(
        mock_token_client.guard_caught_reentrancy(),
        "ReentrancyGuard must intercept recursive call and return Error::Reentrancy"
    );

    // 4. Confirm the guard returned Error::Reentrancy, which has numeric code 4010
    let err_code = mock_token_client.reentry_error_code();
    assert_eq!(
        err_code, 4010,
        "Reentrancy guard must return Error::Reentrancy (code 4010), got: {}",
        err_code
    );
    assert_eq!(
        err_code,
        Error::Reentrancy as u32,
        "Error code must match Error::Reentrancy enum value"
    );

    // 5. State consistency: merchant balance must be deducted exactly once (CEI pattern)
    let expected_remaining = initial_balance - withdraw_amount;
    assert_eq!(
        vault_client.get_merchant_balance(&merchant),
        expected_remaining,
        "Merchant balance must be deducted exactly once; no double-withdrawal"
    );

    // 6. Token balance consistency: merchant received tokens exactly once
    assert_eq!(
        mock_token_client.balance(&merchant),
        withdraw_amount,
        "Merchant must receive tokens from exactly one withdrawal"
    );
    assert_eq!(
        mock_token_client.balance(&vault_id),
        initial_balance - withdraw_amount,
        "Vault token balance must decrease by exactly the single withdrawal amount"
    );

    // 7. Lock release verification: subsequent valid withdrawal must succeed without being blocked
    let second_withdraw: i128 = 20_000_000;
    // Disable re-entry for the second withdrawal to verify clean guard release
    mock_token_client.set_reenter(&false);
    let second_result = vault_client.try_withdraw_merchant_funds(&merchant, &second_withdraw);
    assert!(
        second_result.is_ok(),
        "Guard lock must be released after operation completes so subsequent calls succeed"
    );
    assert_eq!(
        vault_client.get_merchant_balance(&merchant),
        expected_remaining - second_withdraw,
        "Subsequent withdrawal correctly updates remaining balance"
    );
}

// ── Test 2: withdraw_merchant_token_funds Reentrancy Guard Protection ──────────

/// Confirms that `withdraw_merchant_token_funds` also acquires a reentrancy guard
/// before transferring tokens, and that a malicious token attempting a callback
/// re-entry into `withdraw_merchant_token_funds` is blocked with `Error::Reentrancy (4010)`.
#[test]
fn test_withdraw_merchant_token_funds_reentrancy_guard_blocks_callback() {
    let (env, vault_client, vault_id, mock_token_client, token_id, _admin, merchant) =
        setup_reentrancy_test();

    let initial_balance: i128 = 80_000_000;
    let withdraw_amount: i128 = 30_000_000;

    seed_merchant_balance(&env, &vault_id, &merchant, &token_id, initial_balance);
    mock_token_client.set_balance(&vault_id, &initial_balance);

    // Configure mode 1: re-enter `withdraw_merchant_token_funds`
    mock_token_client.set_mode(&1u32);
    mock_token_client.set_reenter(&true);

    let result = vault_client.try_withdraw_merchant_token_funds(
        &merchant,
        &token_id,
        &withdraw_amount,
    );

    assert!(result.is_ok(), "Outer token withdrawal must succeed");
    assert!(
        mock_token_client.callback_executed(),
        "Transfer callback must have fired"
    );
    assert!(
        mock_token_client.guard_caught_reentrancy(),
        "Guard must catch re-entry on withdraw_merchant_token_funds"
    );
    assert_eq!(
        mock_token_client.reentry_error_code(),
        4010,
        "Guard must return Error::Reentrancy (4010)"
    );

    assert_eq!(
        vault_client.get_merchant_balance(&merchant),
        initial_balance - withdraw_amount,
        "Balance deducted only once"
    );
}

// ── Test 3: Double Drain Prevention Verification ─────────────────────────────

/// Confirms that even if the malicious token attempts to drain the ENTIRE remaining
/// merchant balance during the transfer callback, the reentrancy guard rejects it
/// with 4010 and the merchant's balance cannot be drained beyond the approved amount.
#[test]
fn test_reentrancy_callback_cannot_drain_remaining_balance() {
    let (env, vault_client, vault_id, mock_token_client, token_id, _admin, merchant) =
        setup_reentrancy_test();

    let initial_balance: i128 = 100_000_000;
    let withdraw_amount: i128 = 50_000_000;

    seed_merchant_balance(&env, &vault_id, &merchant, &token_id, initial_balance);
    mock_token_client.set_balance(&vault_id, &initial_balance);

    mock_token_client.set_mode(&0u32);
    mock_token_client.set_reenter(&true);

    let result = vault_client.try_withdraw_merchant_funds(&merchant, &withdraw_amount);
    assert!(result.is_ok());

    // Verify guard returned 4010
    assert_eq!(mock_token_client.reentry_error_code(), 4010);

    // Merchant balance must be exactly 50_000_000, not 0
    let balance_after = vault_client.get_merchant_balance(&merchant);
    assert_eq!(
        balance_after, 50_000_000i128,
        "Remaining merchant balance must be preserved against reentrancy drain"
    );
}
