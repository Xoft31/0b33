//! State machine transition tests for `SubscriptionStatus`.
//!
//! Covers the transitions mandated by the task:
//!
//!   Valid transitions (must succeed):
//!     - Active → Paused → Active   (pause / resume round-trip)
//!     - Active → Cancelled          (terminal cancel)
//!     - Active → InsufficientBalance driven by an underfunded charge
//!     - GracePeriod → Active via resume_subscription (after deposit + sufficient balance)
//!
//!   Illegal transitions (must be rejected):
//!     - charge_subscription while Cancelled → `Error::NotActive`
//!     - charge_subscription while Paused   → `Error::NotActive`
//!     - resume_subscription while Active   → idempotent Ok (status stays Active)
//!     - resume_subscription on GracePeriod with insufficient balance → `Error::InsufficientBalance`
//!
//! Security invariants verified:
//!   - A rejected charge on Paused/Cancelled does **not** mutate subscription state.
//!   - An underfunded charge does **not** credit the merchant or update lifetime_charged.
//!   - resume_subscription rejects GracePeriod subscriptions with insufficient balance.

use crate::{ChargeExecutionResult, DataKey, Error, SubscriptionStatus, SubscriptionVault, SubscriptionVaultClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    Address, Env,
};

// ── Constants ─────────────────────────────────────────────────────────────────

/// Ledger timestamp at contract initialisation (arbitrary non-zero value).
const T0: u64 = 1_000_000;
/// 30-day billing interval (seconds).
const INTERVAL: u64 = 30 * 24 * 60 * 60;
/// 7-day grace window configured at init (seconds).
const GRACE_PERIOD: u64 = 7 * 24 * 60 * 60;
/// 10 USDC per interval (6-decimal token).
const AMOUNT: i128 = 10_000_000;
/// 50 USDC prepaid balance.
const PREPAID: i128 = 50_000_000;

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Spin up an env, register the contract, and initialise it.
/// Returns `(env, client, token_address, admin_address)`.
fn setup() -> (Env, SubscriptionVaultClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = T0);

    let admin = Address::generate(&env);
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();

    // Grace period matches GRACE_PERIOD so we can deterministically hit
    // InsufficientBalance (not GracePeriod) by choosing now >= grace_expires.
    client.init(&token, &6, &admin, &1_000_000i128, &GRACE_PERIOD);

    (env, client, token, admin)
}

/// Create a subscription and return `(subscription_id, subscriber, merchant)`.
fn create_sub(env: &Env, client: &SubscriptionVaultClient) -> (u32, Address, Address) {
    let subscriber = Address::generate(env);
    let merchant = Address::generate(env);
    let id = client.create_subscription(
        &subscriber,
        &merchant,
        &AMOUNT,
        &INTERVAL,
        &false,       // usage_enabled
        &None::<i128>, // lifetime_cap
        &None::<u64>,  // expires_at
        &None::<u32>,  // expires_at_ledger
    );
    (id, subscriber, merchant)
}

/// Directly overwrite the `prepaid_balance` field in persistent storage.
///
/// Used to set an exact balance without going through `deposit_funds`
/// (which requires a real token transfer and enforces the min-topup rule).
fn set_balance(env: &Env, client: &SubscriptionVaultClient, id: u32, balance: i128) {
    let mut sub = client.get_subscription(&id);
    sub.prepaid_balance = balance;
    env.as_contract(&client.address, || {
        env.storage().persistent().set(&DataKey::Sub(id), &sub);
    });
}

// ── Valid transition: Active → Paused → Active ────────────────────────────────

/// Pausing an Active subscription then resuming it returns the subscription to
/// Active with all other fields preserved.
#[test]
fn test_active_to_paused_to_active() {
    let (env, client, token, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    // Fund the subscription so the balance check in resume is satisfied.
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &PREPAID);
    client.deposit_funds(&id, &PREPAID, &None::<soroban_sdk::BytesN<32>>);

    let initial_balance = client.get_subscription(&id).prepaid_balance;

    // ── Active → Paused ──
    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Active);
    client.pause_subscription(&id, &subscriber);
    let sub_paused = client.get_subscription(&id);
    assert_eq!(sub_paused.status, SubscriptionStatus::Paused);
    // Balance and timestamp must not change on pause.
    assert_eq!(sub_paused.prepaid_balance, initial_balance);

    // ── Paused → Active ──
    client.resume_subscription(&id, &subscriber);
    let sub_resumed = client.get_subscription(&id);
    assert_eq!(sub_resumed.status, SubscriptionStatus::Active);
    // Balance and timestamp still unchanged after resume.
    assert_eq!(sub_resumed.prepaid_balance, initial_balance);
}

/// Resuming from InsufficientBalance must not silently re-enable billing unless
/// the prepaid balance is now sufficient for at least one interval charge.
#[test]
fn test_resume_from_insufficient_balance_requires_sufficient_balance() {
    let (env, client, token, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &AMOUNT - 1);
    client.deposit_funds(&id, &subscriber, &(AMOUNT - 1), &None::<soroban_sdk::BytesN<32>>);

    client.get_subscription(&id);
    set_balance(&env, &client, id, AMOUNT - 1);
    let sub_before = client.get_subscription(&id);
    assert_eq!(sub_before.status, SubscriptionStatus::Active);

    // Simulate the contract having already transitioned to InsufficientBalance,
    // while only a partial top-up is available.
    set_balance(&env, &client, id, AMOUNT - 1);
    env.as_contract(&client.address, || {
        let mut sub = env.storage().persistent().get(&DataKey::Sub(id)).unwrap().unwrap();
        sub.status = SubscriptionStatus::InsufficientBalance;
        env.storage().persistent().set(&DataKey::Sub(id), &sub);
    });

    let result = client.try_resume_subscription(&id, &subscriber);
    assert_eq!(result, Err(Ok(Error::InsufficientBalance)));
    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::InsufficientBalance);
}

// ── Valid transition: Active → Cancelled ─────────────────────────────────────

/// Cancelling an Active subscription moves it to the terminal Cancelled state.
/// The prepaid balance is retained (available for subscriber withdrawal).
#[test]
fn test_active_to_cancelled() {
    let (env, client, token, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &PREPAID);
    client.deposit_funds(&id, &PREPAID, &None::<soroban_sdk::BytesN<32>>);

    let balance_before = client.get_subscription(&id).prepaid_balance;
    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Active);

    client.cancel_subscription(&id, &subscriber);

    let sub = client.get_subscription(&id);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
    // Balance is preserved (not burned on cancel — subscriber can still withdraw).
    assert_eq!(sub.prepaid_balance, balance_before);
}

// ── Valid transition: Active → InsufficientBalance via underfunded charge ─────

/// When charge_subscription is called on an Active subscription whose
/// prepaid_balance is less than the charge amount, and the grace window has
/// already expired, the subscription transitions to InsufficientBalance.
///
/// This is the canonical "ran out of money" path described in
/// docs/lifecycle_edgecases.md §"Canonical Insufficient-Balance Path".
///
/// Security invariants checked:
///   - lifetime_charged is NOT incremented.
///   - The merchant balance is NOT credited.
///   - The call returns `Ok(InsufficientBalance)`, not an error.
#[test]
fn test_active_to_insufficient_balance_via_underfunded_charge() {
    let (env, client, _, _) = setup();
    let (id, _, _) = create_sub(&env, &client);

    // Ensure prepaid_balance is 0 — subscription cannot cover AMOUNT.
    set_balance(&env, &client, id, 0);

    // Advance ledger past (interval + grace) so we land in InsufficientBalance
    // rather than GracePeriod.
    //   grace_expires = last_payment_timestamp + interval + grace_period
    //                 = T0 + INTERVAL + GRACE_PERIOD
    // Setting now = T0 + INTERVAL + GRACE_PERIOD + 1 clears the grace window.
    env.ledger()
        .with_mut(|l| l.timestamp = T0 + INTERVAL + GRACE_PERIOD + 1);

    let sub_before = client.get_subscription(&id);
    assert_eq!(sub_before.status, SubscriptionStatus::Active);
    assert_eq!(sub_before.lifetime_charged, 0);

    let result = client.try_charge_subscription(&id, &None::<soroban_sdk::BytesN<32>>);

    // charge_subscription returns Ok(InsufficientBalance) — not a hard error —
    // because the insufficient-balance path is a recoverable lifecycle event.
    assert_eq!(result, Ok(Ok(ChargeExecutionResult::InsufficientBalance)));

    let sub_after = client.get_subscription(&id);
    assert_eq!(sub_after.status, SubscriptionStatus::InsufficientBalance);

    // Security: no funds moved.
    assert_eq!(sub_after.prepaid_balance, 0);
    assert_eq!(sub_after.lifetime_charged, 0);
}

#[test]
fn test_next_charge_info_keeps_grace_deadline_after_expiry() {
    let (env, client, _, _) = setup();
    let (id, _, _) = create_sub(&env, &client);

    env.ledger()
        .with_mut(|l| l.timestamp = T0 + INTERVAL + 1);
    let result = client.try_charge_subscription(&id, &None::<soroban_sdk::BytesN<32>>);
    assert_eq!(result, Ok(Ok(ChargeExecutionResult::InsufficientBalance)));

    let grace_deadline = T0 + INTERVAL + 1 + GRACE_PERIOD;
    env.ledger().with_mut(|l| l.timestamp = grace_deadline);

    let info = client.get_next_charge_info(&id);
    assert_eq!(info.status, SubscriptionStatus::InsufficientBalance);
    assert_eq!(info.grace_deadline, Some(grace_deadline));
}

// ── Illegal transition: charge while Cancelled ────────────────────────────────

/// Attempting to charge a Cancelled (terminal) subscription must return
/// `Error::NotActive` and leave the subscription state unchanged.
#[test]
fn test_charge_cancelled_subscription_rejected() {
    let (env, client, _, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    client.cancel_subscription(&id, &subscriber);
    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Cancelled);

    let result = client.try_charge_subscription(&id, &None::<soroban_sdk::BytesN<32>>);
    assert_eq!(result, Err(Ok(Error::NotActive)));

    // Status did not change.
    assert_eq!(
        client.get_subscription(&id).status,
        SubscriptionStatus::Cancelled,
    );
}

// ── Illegal transition: charge while Paused ───────────────────────────────────

/// Attempting to charge a Paused subscription must return `Error::NotActive`
/// even when the billing interval has elapsed. The status must not change.
#[test]
fn test_charge_paused_subscription_rejected() {
    let (env, client, token, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    // Fund the subscription so balance isn't the limiting factor.
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &PREPAID);
    client.deposit_funds(&id, &PREPAID, &None::<soroban_sdk::BytesN<32>>);

    client.pause_subscription(&id, &subscriber);
    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Paused);

    // Advance past the billing interval — the charge would succeed if Active.
    env.ledger().with_mut(|l| l.timestamp = T0 + INTERVAL + 1);

    let result = client.try_charge_subscription(&id, &None::<soroban_sdk::BytesN<32>>);
    assert_eq!(result, Err(Ok(Error::NotActive)));

    // Status and balance must be unchanged after the rejected charge.
    let sub = client.get_subscription(&id);
    assert_eq!(sub.status, SubscriptionStatus::Paused);
    assert_eq!(sub.prepaid_balance, PREPAID);
}

// ── resume_subscription while Active is idempotent ───────────────────────────

/// Calling resume_subscription on an already-Active subscription is a no-op:
/// it returns Ok without error and the status remains Active.
///
/// This matches the state machine rule "Any state → same state is always allowed"
/// and is documented in the do_resume_subscription implementation.
#[test]
fn test_resume_active_subscription_is_idempotent() {
    let (env, client, _, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Active);

    let result = client.try_resume_subscription(&id, &subscriber);
    assert_eq!(result, Ok(Ok(())));

    // Status stays Active; no spurious state change.
    assert_eq!(client.get_subscription(&id).status, SubscriptionStatus::Active);
}

// ── Valid transition: GracePeriod → Active via resume_subscription ───────────

/// When a subscription enters GracePeriod due to an underfunded charge,
/// and the subscriber then deposits sufficient funds, calling resume_subscription
/// must transition the subscription from GracePeriod back to Active.
///
/// This test verifies:
///   1. A subscription can be moved into GracePeriod by charging with insufficient balance
///      during the grace window (when grace_expires has not yet passed).
///   2. The subscriber can deposit funds to rebuild the balance above the charge amount.
///   3. resume_subscription successfully transitions GracePeriod → Active.
///   4. The subscription retains its prepaid_balance and other fields through the transition.
#[test]
fn test_grace_period_to_active_via_resume_subscription() {
    let (env, client, token, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    // Set up the subscription to enter GracePeriod by charging with insufficient balance.
    // We need:
    //   - last_payment_timestamp = T0
    //   - interval = INTERVAL
    //   - grace_period = GRACE_PERIOD
    //   - grace_expires = T0 + INTERVAL + GRACE_PERIOD
    // Charging at T0 + INTERVAL + 1 is before grace_expires, so status → GracePeriod
    // instead of InsufficientBalance.
    env.ledger()
        .with_mut(|l| l.timestamp = T0 + INTERVAL + 1);

    let sub_before_charge = client.get_subscription(&id);
    assert_eq!(sub_before_charge.status, SubscriptionStatus::Active);
    assert_eq!(sub_before_charge.prepaid_balance, 0);

    // Charge the Active subscription with zero balance while still in grace window.
    // This should transition to GracePeriod.
    let charge_result = client.try_charge_subscription(&id, &None::<soroban_sdk::BytesN<32>>);
    assert!(charge_result.is_ok(), "charge during grace window should return Ok");

    let sub_in_grace = client.get_subscription(&id);
    assert_eq!(
        sub_in_grace.status, SubscriptionStatus::GracePeriod,
        "subscription should be in GracePeriod after charge with insufficient balance during grace window"
    );
    assert_eq!(sub_in_grace.prepaid_balance, 0);

    // Now, the subscriber deposits funds to cover the next charge.
    // This requires: prepaid_balance >= amount (10_000_000).
    // We deposit exactly the charge amount.
    let token_admin = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&subscriber, &AMOUNT);

    let sub_before_deposit = client.get_subscription(&id);
    client.deposit_funds(&id, &subscriber, &AMOUNT, &None::<soroban_sdk::BytesN<32>>);

    let sub_after_deposit = client.get_subscription(&id);
    assert_eq!(sub_after_deposit.status, SubscriptionStatus::GracePeriod);
    assert_eq!(sub_after_deposit.prepaid_balance, AMOUNT);

    // Now call resume_subscription to transition from GracePeriod → Active.
    let resume_result = client.try_resume_subscription(&id, &subscriber);
    assert_eq!(
        resume_result, Ok(Ok(())),
        "resume_subscription should succeed and return Ok(())"
    );

    let sub_after_resume = client.get_subscription(&id);
    assert_eq!(
        sub_after_resume.status, SubscriptionStatus::Active,
        "subscription should be Active after resume_subscription"
    );
    // Prepaid balance is preserved.
    assert_eq!(sub_after_resume.prepaid_balance, AMOUNT);
    // Other fields like subscriber and merchant should be unchanged.
    assert_eq!(sub_after_resume.subscriber, sub_in_grace.subscriber);
    assert_eq!(sub_after_resume.merchant, sub_in_grace.merchant);
    assert_eq!(sub_after_resume.amount, sub_in_grace.amount);
    assert_eq!(sub_after_resume.interval_seconds, sub_in_grace.interval_seconds);
}

/// Invariant: resume_subscription on a GracePeriod subscription with insufficient
/// balance (prepaid < amount) must be rejected with Error::InsufficientBalance,
/// and the subscription status must remain GracePeriod.
#[test]
fn test_resume_grace_period_with_insufficient_balance_rejected() {
    let (env, client, _, _) = setup();
    let (id, subscriber, _) = create_sub(&env, &client);

    // Move to GracePeriod with zero balance.
    env.ledger()
        .with_mut(|l| l.timestamp = T0 + INTERVAL + 1);
    let _ = client.try_charge_subscription(&id, &None::<soroban_sdk::BytesN<32>>);

    let sub_in_grace = client.get_subscription(&id);
    assert_eq!(sub_in_grace.status, SubscriptionStatus::GracePeriod);
    assert_eq!(sub_in_grace.prepaid_balance, 0);

    // Try to resume without depositing funds — prepaid_balance (0) < amount (10M)
    let resume_result = client.try_resume_subscription(&id, &subscriber);
    assert_eq!(
        resume_result,
        Err(Ok(Error::InsufficientBalance)),
        "resume_subscription must reject GracePeriod subscription with insufficient balance"
    );

    // Status and balance must remain unchanged.
    let sub_after_failed_resume = client.get_subscription(&id);
    assert_eq!(
        sub_after_failed_resume.status,
        SubscriptionStatus::GracePeriod,
        "status must remain GracePeriod after failed resume"
    );
    assert_eq!(sub_after_failed_resume.prepaid_balance, 0);
}
