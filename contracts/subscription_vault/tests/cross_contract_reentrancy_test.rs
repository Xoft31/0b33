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

/// Target entrypoint for cross-contract reentrancy attack simulation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AttackTarget {
    DepositFunds,
    WithdrawMerchantFunds,
}

/// Mock malicious token contract that attempts cross-contract reentrancy during `transfer`.
///
/// In Soroban's invocation model, external cross-contract calls pause the caller's frame
/// while the callee executes. If a reentrancy guard were stored in non-persistent or
/// improperly scoped storage, a callback could fail to observe the lock.
///
/// This contract tests that:
/// 1. When `deposit_funds` calls `token.transfer()`, the callback cannot re-enter `deposit_funds`.
/// 2. When `withdraw_merchant_funds` calls `token.transfer()`, the callback cannot re-enter `withdraw_merchant_funds`.
/// 3. In both scenarios, the persistent-storage reentrancy guard intercepts the nested call
///    and immediately returns `Error::Reentrancy` (error code 4010).
#[contract]
pub struct CrossContractMaliciousToken;

#[contractimpl]
impl CrossContractMaliciousToken {
    /// Initialize with vault address reference.
    pub fn init(env: Env, vault: Address) {
        env.storage().instance().set(&Symbol::new(&env, "vault"), &vault);
        env.storage().instance().set(&Symbol::new(&env, "reenter_active"), &true);
        env.storage().instance().set(&Symbol::new(&env, "target_entrypoint"), &0u32); // 0 = Deposit, 1 = WithdrawMerchant
        env.storage().instance().set(&Symbol::new(&env, "sub_id"), &0u32);
        env.storage().instance().set(&Symbol::new(&env, "callback_fired"), &false);
        env.storage().instance().set(&Symbol::new(&env, "reentry_blocked"), &false);
        env.storage().instance().set(&Symbol::new(&env, "intercepted_err_code"), &0u32);
    }

    /// Configure attack target: 0 for deposit_funds, 1 for withdraw_merchant_funds.
    pub fn set_attack_target(env: Env, target_code: u32, sub_id: u32) {
        env.storage().instance().set(&Symbol::new(&env, "target_entrypoint"), &target_code);
        env.storage().instance().set(&Symbol::new(&env, "sub_id"), &sub_id);
    }

    /// Enable or disable re-entry callback.
    pub fn set_reenter_active(env: Env, active: bool) {
        env.storage().instance().set(&Symbol::new(&env, "reenter_active"), &active);
    }

    /// Set balance for an address in mock storage.
    pub fn set_balance(env: Env, account: Address, amount: i128) {
        let key = Symbol::new(&env, "bal");
        env.storage().persistent().set(&(key, account), &amount);
    }

    /// SEP-41 balance function.
    pub fn balance(env: Env, id: Address) -> i128 {
        let key = Symbol::new(&env, "bal");
        env.storage().persistent().get(&(key, id)).unwrap_or(0i128)
    }

    /// SEP-41 transfer function simulating a cross-contract callback attack.
    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        from.require_auth();

        // 1. Update balances inside token
        let bal_key = Symbol::new(&env, "bal");
        let from_bal: i128 = env.storage().persistent().get(&(bal_key.clone(), from.clone())).unwrap_or(0i128);
        let to_bal: i128 = env.storage().persistent().get(&(bal_key.clone(), to.clone())).unwrap_or(0i128);
        env.storage().persistent().set(&(bal_key.clone(), from.clone()), &(from_bal - amount));
        env.storage().persistent().set(&(bal_key, to.clone()), &(to_bal + amount));

        // 2. Execute cross-contract reentrancy callback if enabled
        let reenter_active: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "reenter_active"))
            .unwrap_or(false);

        let recursion_guard: bool = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "in_recursion"))
            .unwrap_or(false);

        if reenter_active && !recursion_guard {
            env.storage().instance().set(&Symbol::new(&env, "in_recursion"), &true);
            env.storage().instance().set(&Symbol::new(&env, "callback_fired"), &true);

            let vault_addr: Address = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "vault"))
                .unwrap_or_else(|| {
                    // In a deposit, `to` is the vault; in a withdrawal, `from` is the vault
                    if to != from {
                        to.clone()
                    } else {
                        from.clone()
                    }
                });

            let target_code: u32 = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "target_entrypoint"))
                .unwrap_or(0u32);

            let sub_id: u32 = env
                .storage()
                .instance()
                .get(&Symbol::new(&env, "sub_id"))
                .unwrap_or(0u32);

            let vault_client = SubscriptionVaultClient::new(&env, &vault_addr);

            // Trigger re-entry attempt based on configured attack target
            let attack_result = if target_code == 0 {
                // Cross-contract callback re-entering `deposit_funds`
                vault_client.try_deposit_funds(&sub_id, &amount, &None::<u64>)
            } else {
                // Cross-contract callback re-entering `withdraw_merchant_funds`
                // In withdrawal, `to` is the merchant
                vault_client.try_withdraw_merchant_funds(&to, &amount)
            };

            // Inspect the result of the re-entry invocation
            match attack_result {
                Err(Ok(Error::Reentrancy)) => {
                    // ReentrancyGuard in persistent storage successfully blocked the re-entry!
                    env.storage().instance().set(&Symbol::new(&env, "reentry_blocked"), &true);
                    env.storage().instance().set(&Symbol::new(&env, "intercepted_err_code"), &4010u32);
                }
                Ok(_) => {
                    // Critical failure: Reentrancy guard was bypassed!
                    env.storage().instance().set(&Symbol::new(&env, "reentry_blocked"), &false);
                    env.storage().instance().set(&Symbol::new(&env, "intercepted_err_code"), &0u32);
                }
                Err(Ok(err)) => {
                    // Other contract error
                    env.storage().instance().set(&Symbol::new(&env, "reentry_blocked"), &false);
                    env.storage().instance().set(&Symbol::new(&env, "intercepted_err_code"), &(err as u32));
                }
                Err(Err(_)) => {
                    // Host invocation error
                    env.storage().instance().set(&Symbol::new(&env, "reentry_blocked"), &false);
                    env.storage().instance().set(&Symbol::new(&env, "intercepted_err_code"), &9999u32);
                }
            }

            env.storage().instance().set(&Symbol::new(&env, "in_recursion"), &false);
        }
    }

    /// Returns whether the callback was fired during transfer.
    pub fn callback_fired(env: Env) -> bool {
        env.storage().instance().get(&Symbol::new(&env, "callback_fired")).unwrap_or(false)
    }

    /// Returns whether the re-entry call was blocked by Error::Reentrancy.
    pub fn reentry_blocked(env: Env) -> bool {
        env.storage().instance().get(&Symbol::new(&env, "reentry_blocked")).unwrap_or(false)
    }

    /// Returns the error code intercepted during re-entry (expected: 4010).
    pub fn intercepted_err_code(env: Env) -> u32 {
        env.storage().instance().get(&Symbol::new(&env, "intercepted_err_code")).unwrap_or(0u32)
    }
}

/// Helper fixture to set up test environment with SubscriptionVault and CrossContractMaliciousToken.
fn setup_env() -> (
    Env,
    SubscriptionVaultClient<'static>,
    Address,
    CrossContractMaliciousTokenClient<'static>,
    Address,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);

    let vault_id = env.register(SubscriptionVault, ());
    let vault_client = SubscriptionVaultClient::new(&env, &vault_id);

    let token_id = env.register(CrossContractMaliciousToken, ());
    let token_client = CrossContractMaliciousTokenClient::new(&env, &token_id);
    token_client.init(&vault_id);

    let min_topup: i128 = 1_000_000;
    let grace_period: u64 = 7 * 24 * 60 * 60;

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

    (
        env,
        vault_client,
        vault_id,
        token_client,
        token_id,
        admin,
        subscriber,
        merchant,
    )
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

// ── Test 1: Cross-contract Reentrancy Attack Blocked During Deposit ───────────

/// Confirms that when a user calls `deposit_funds`, and the malicious token tries to
/// re-enter `deposit_funds` during `token.transfer()`, the persistent storage
/// ReentrancyGuard intercepts the callback and returns `Error::Reentrancy` (4010).
#[test]
fn test_cross_contract_reentrancy_during_deposit_blocked_by_guard() {
    let (env, vault_client, _vault_id, token_client, token_id, _admin, subscriber, merchant) =
        setup_env();

    // Create a subscription
    let sub_amount: i128 = 10_000_000; // 10 USDC
    let interval: u64 = 30 * 24 * 60 * 60; // 30 days
    let sub_id = vault_client.create_subscription(
        &subscriber,
        &merchant,
        &sub_amount,
        &interval,
        &false,          // usage_enabled
        &None::<i128>,   // lifetime_cap
        &None::<u64>,    // expires_at
        &None::<u32>,    // expires_at_ledger
        &None::<Symbol>, // sub_account_label
        &false,          // proration_enabled
    );

    // Mint tokens to subscriber
    let deposit_amount: i128 = 25_000_000;
    token_client.set_balance(&subscriber, &100_000_000);

    // Configure malicious token to attack `deposit_funds` (target code 0) for this sub_id
    token_client.set_attack_target(&0u32, &sub_id);
    token_client.set_reenter_active(&true);

    // Execute deposit_funds — this invokes token.transfer(), which executes the re-entry callback
    let result = vault_client.try_deposit_funds(&sub_id, &deposit_amount, &None::<u64>);

    // 1. The outer deposit must succeed
    assert!(result.is_ok(), "Outer deposit operation should succeed");

    // 2. The malicious callback must have executed during token.transfer()
    assert!(
        token_client.callback_fired(),
        "Malicious token transfer callback must have fired"
    );

    // 3. The re-entry attempt must have been blocked by ReentrancyGuard
    assert!(
        token_client.reentry_blocked(),
        "Cross-contract re-entry callback must be blocked by the reentrancy guard"
    );

    // 4. The guard must have returned Error::Reentrancy (4010)
    let intercepted_code = token_client.intercepted_err_code();
    assert_eq!(
        intercepted_code, 4010,
        "Reentrancy guard must return Error::Reentrancy (code 4010), got: {}",
        intercepted_code
    );
    assert_eq!(
        intercepted_code,
        Error::Reentrancy as u32,
        "Intercepted error code must match Error::Reentrancy discriminant"
    );

    // 5. Invariant check: prepaid balance must be credited exactly ONCE (no double-credit)
    let sub = vault_client.get_subscription(&sub_id);
    assert_eq!(
        sub.prepaid_balance, deposit_amount,
        "Subscription prepaid balance must reflect exactly one deposit (no double-crediting)"
    );

    // 6. Token balance check: subscriber must be debited exactly ONCE
    let remaining_subscriber_tokens = token_client.balance(&subscriber);
    assert_eq!(
        remaining_subscriber_tokens,
        100_000_000 - deposit_amount,
        "Subscriber must only be debited for the single authorized deposit"
    );

    // 7. Lock release check: subsequent valid deposit succeeds without being blocked by stale lock
    token_client.set_reenter_active(&false);
    let second_deposit = vault_client.try_deposit_funds(&sub_id, &10_000_000, &None::<u64>);
    assert!(
        second_deposit.is_ok(),
        "Guard lock must be released when scope exits, allowing subsequent deposits"
    );
    let sub_after = vault_client.get_subscription(&sub_id);
    assert_eq!(
        sub_after.prepaid_balance,
        deposit_amount + 10_000_000,
        "Second deposit correctly increases balance"
    );
}

// ── Test 2: Cross-contract Reentrancy Attack Blocked During Merchant Withdrawal ─

/// Confirms that when a merchant calls `withdraw_merchant_funds`, and the malicious
/// token tries to re-enter `withdraw_merchant_funds` during `token.transfer()`, the
/// persistent storage ReentrancyGuard intercepts the callback and returns `Error::Reentrancy` (4010).
#[test]
fn test_cross_contract_reentrancy_during_merchant_withdraw_blocked() {
    let (env, vault_client, vault_id, token_client, token_id, _admin, _subscriber, merchant) =
        setup_env();

    let initial_balance: i128 = 50_000_000;
    let withdraw_amount: i128 = 20_000_000;

    // Seed merchant earnings and fund vault with tokens
    seed_merchant_balance(&env, &vault_id, &merchant, &token_id, initial_balance);
    token_client.set_balance(&vault_id, &initial_balance);

    // Configure malicious token to attack `withdraw_merchant_funds` (target code 1)
    token_client.set_attack_target(&1u32, &0u32);
    token_client.set_reenter_active(&true);

    // Trigger merchant withdrawal
    let result = vault_client.try_withdraw_merchant_funds(&merchant, &withdraw_amount);

    // 1. Outer withdrawal must succeed
    assert!(result.is_ok(), "Outer withdrawal should succeed");

    // 2. Callback executed
    assert!(
        token_client.callback_fired(),
        "Transfer callback must have executed"
    );

    // 3. Re-entry blocked by guard
    assert!(
        token_client.reentry_blocked(),
        "Re-entry during withdrawal must be blocked by ReentrancyGuard"
    );

    // 4. Confirms error code 4010 (Error::Reentrancy)
    let err_code = token_client.intercepted_err_code();
    assert_eq!(
        err_code, 4010,
        "Guard must return Error::Reentrancy (4010), got: {}",
        err_code
    );

    // 5. State consistency: merchant balance reduced only once (CEI pattern holds)
    let merchant_bal = vault_client.get_merchant_balance(&merchant);
    assert_eq!(
        merchant_bal,
        initial_balance - withdraw_amount,
        "Merchant balance must be reduced exactly once"
    );

    // 6. Token balance consistency
    assert_eq!(
        token_client.balance(&merchant),
        withdraw_amount,
        "Merchant should receive tokens from exactly one withdrawal"
    );
}

// ── Test 3: Guard Survives Cross-Contract Boundary Because of Persistent Storage ─

/// Demonstrates that storing the guard key in persistent storage (rather than
/// instance storage) guarantees that any cross-contract invocation sees the active
/// lock regardless of Soroban frame switches.
#[test]
fn test_reentrancy_guard_persistent_storage_consistency() {
    let (env, vault_client, vault_id, token_client, token_id, _admin, subscriber, merchant) =
        setup_env();

    let sub_id = vault_client.create_subscription(
        &subscriber,
        &merchant,
        &5_000_000,
        &86_400,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<Symbol>,
        &false,
    );

    token_client.set_balance(&subscriber, &50_000_000);
    token_client.set_attack_target(&0u32, &sub_id);
    token_client.set_reenter_active(&true);

    // Execute guarded operation with malicious token
    let res = vault_client.try_deposit_funds(&sub_id, &10_000_000, &None::<u64>);
    assert!(res.is_ok());

    // Verify callback was caught with Reentrancy (4010)
    assert!(token_client.callback_fired());
    assert!(token_client.reentry_blocked());
    assert_eq!(token_client.intercepted_err_code(), 4010);

    // Verify that after transaction completes, the lock is completely cleared from persistent storage
    env.as_contract(&vault_id, || {
        let key = Symbol::new(&env, "deposit_funds");
        assert!(
            !env.storage().persistent().has(&key),
            "Persistent reentrancy lock key must be removed upon scope exit"
        );
    });
}

// ── Test 4: Soroban Invocation Model Cross-Contract Reentrancy Simulation ─────

/// Rigorously verifies that Soroban's cross-contract invocation model cannot bypass
/// the reentrancy guard.
///
/// Under Soroban's execution semantics:
/// 1. Outer Frame: Vault contract executes `withdraw_merchant_funds`, acquires the
///    reentrancy lock in persistent storage, updates internal balance (CEI pattern),
///    and invokes `token.transfer()`.
/// 2. Intermediate Frame: VM pauses the vault frame and enters the token contract.
///    The malicious token contract attempts to call back into `vault.withdraw_merchant_funds()`.
/// 3. Nested Frame: A new contract frame for the vault contract is instantiated.
///    The vault calls `ReentrancyGuard::lock("withdraw_merchant_funds")`.
///    Because the lock is held in persistent storage, the lock key is immediately
///    visible across invocation boundaries.
/// 4. Protection Verification:
///    - The nested call receives `Err(Ok(Error::Reentrancy))` (error code 4010).
///    - The outer call resumes and completes cleanly.
///    - Accounting invariants are strictly preserved: balance deducted once, transferred once.
///    - The lock is cleanly cleared upon final scope exit.
#[test]
fn test_cross_contract_reentrancy_cannot_bypass_guard_under_soroban_invocation_model() {
    let (env, vault_client, vault_id, token_client, token_id, _admin, _subscriber, merchant) =
        setup_env();

    let initial_merchant_vault_balance: i128 = 75_000_000;
    let withdrawal_amount: i128 = 25_000_000;

    // Credit merchant and supply vault with tokens
    seed_merchant_balance(
        &env,
        &vault_id,
        &merchant,
        &token_id,
        initial_merchant_vault_balance,
    );
    token_client.set_balance(&vault_id, &initial_merchant_vault_balance);

    // Verify initial conditions
    assert_eq!(vault_client.get_merchant_balance(&merchant), initial_merchant_vault_balance);
    assert_eq!(token_client.balance(&vault_id), initial_merchant_vault_balance);
    assert_eq!(token_client.balance(&merchant), 0);

    // Configure malicious token to attack `withdraw_merchant_funds` (target code 1)
    token_client.set_attack_target(&1u32, &0u32);
    token_client.set_reenter_active(&true);

    // Execute withdrawal — triggers cross-contract callback
    let result = vault_client.try_withdraw_merchant_funds(&merchant, &withdrawal_amount);

    // 1. Outer withdrawal completes successfully
    assert!(result.is_ok(), "Outer withdrawal must succeed without being aborted");

    // 2. Malicious token transfer callback fired in intermediate invocation frame
    assert!(
        token_client.callback_fired(),
        "Malicious token callback must execute during transfer"
    );

    // 3. Nested re-entry frame was blocked by ReentrancyGuard
    assert!(
        token_client.reentry_blocked(),
        "Nested call into vault must be blocked by the reentrancy guard"
    );

    // 4. Intercepted error must be Error::Reentrancy (4010)
    assert_eq!(
        token_client.intercepted_err_code(),
        4010,
        "Intercepted error must be Error::Reentrancy (4010)"
    );

    // 5. Merchant balance in vault was debited exactly once
    assert_eq!(
        vault_client.get_merchant_balance(&merchant),
        initial_merchant_vault_balance - withdrawal_amount,
        "Merchant balance must be debited exactly once"
    );

    // 6. Token balance received by merchant is exactly one withdrawal amount
    assert_eq!(
        token_client.balance(&merchant),
        withdrawal_amount,
        "Merchant token balance must reflect exactly one withdrawal"
    );

    // 7. Vault token balance reduced by exactly one withdrawal amount
    assert_eq!(
        token_client.balance(&vault_id),
        initial_merchant_vault_balance - withdrawal_amount,
        "Vault token balance must decrease by exactly one withdrawal"
    );

    // 8. Reentrancy lock in persistent storage is released after outer frame completes
    env.as_contract(&vault_id, || {
        let key = Symbol::new(&env, "withdraw_merchant_funds");
        assert!(
            !env.storage().persistent().has(&key),
            "Reentrancy lock must be removed from persistent storage when outer scope ends"
        );
    });

    // 9. Subsequent withdrawal succeeds cleanly without residual lock
    token_client.set_reenter_active(&false);
    let subsequent_result =
        vault_client.try_withdraw_merchant_funds(&merchant, &withdrawal_amount);
    assert!(
        subsequent_result.is_ok(),
        "Subsequent withdrawal must succeed cleanly after lock release"
    );
    assert_eq!(
        vault_client.get_merchant_balance(&merchant),
        initial_merchant_vault_balance - (withdrawal_amount * 2),
        "Subsequent withdrawal correctly debits merchant balance"
    );
}

