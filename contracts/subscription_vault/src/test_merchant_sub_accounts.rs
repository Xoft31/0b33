//! Tests for merchant sub-accounts (#575).
//!
//! Covers registration, duplicate/empty-label rejection, pre-registration
//! guard, getters, withdrawal, event emission, balance independence,
//! subscription-level sub_account_label binding, and end-to-end charge
//! routing to sub-accounts (interval, usage, and one-off).

use crate::{Error, SubscriptionVault, SubscriptionVaultClient};
use soroban_sdk::testutils::{Address as _, Events as _};
use soroban_sdk::{Address, Env, Symbol, Vec};

fn label(env: &Env, s: &str) -> Symbol {
    Symbol::new(env, s)
}

/// Minimal setup: init contract, return (env, client, admin, token).
fn setup() -> (Env, SubscriptionVaultClient<'static>, Address, Address) {
    let env = Env::default();
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.init(&token, &6, &admin, &1_000_000i128, &(7 * 24 * 60 * 60));
    (env, client, admin, token)
}

/// Create a merchant with an initialized config and return its address.
fn create_merchant(
    client: &SubscriptionVaultClient<'static>,
    env: &Env,
) -> Address {
    let merchant = Address::generate(env);
    let payout = Address::generate(env);
    client.initialize_merchant_config(
        &merchant,
        &payout,
        &0,
        &1,
        &None,
        &soroban_sdk::String::from_str(env, ""),
    );
    merchant
}

// ── Registration ────────────────────────────────────────────────────────────

#[test]
fn register_sub_account_succeeds() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "sales");

    client.register_sub_account(&merchant, &lbl);

    assert_eq!(client.get_sub_account_list(&merchant), Vec::from_array(&env, [lbl.clone()]));
    assert_eq!(client.get_sub_account_balance(&merchant, &lbl), 0);
}

#[test]
fn register_multiple_sub_accounts() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);

    let sales = label(&env, "sales");
    let support = label(&env, "support");

    client.register_sub_account(&merchant, &sales);
    client.register_sub_account(&merchant, &support);

    let list = client.get_sub_account_list(&merchant);
    assert_eq!(list.len(), 2);
    assert!(list.contains(&sales));
    assert!(list.contains(&support));
}

#[test]
fn duplicate_label_rejected() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "sales");

    client.register_sub_account(&merchant, &lbl);
    let result = client.try_register_sub_account(&merchant, &lbl);
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn empty_label_rejected() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    let empty = label(&env, "");

    let result = client.try_register_sub_account(&merchant, &empty);
    assert_eq!(result, Err(Ok(Error::InvalidInput)));
}

#[test]
fn uninitialized_merchant_cannot_register_sub_account() {
    let (env, client, _admin, _token) = setup();
    let merchant = Address::generate(&env);
    let lbl = label(&env, "sales");

    let result = client.try_register_sub_account(&merchant, &lbl);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

// ── Getters ──────────────────────────────────────────────────────────────────

#[test]
fn sub_account_list_defaults_to_empty() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    assert_eq!(client.get_sub_account_list(&merchant).len(), 0);
}

#[test]
fn sub_account_balance_defaults_to_zero() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "sales");
    assert_eq!(client.get_sub_account_balance(&merchant, &lbl), 0);
}

#[test]
fn sub_account_balance_zero_for_unregistered_label() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "ghost");
    assert_eq!(client.get_sub_account_balance(&merchant, &lbl), 0);
}

// ── Withdrawal ───────────────────────────────────────────────────────────────

#[test]
fn withdraw_sub_account_funds_succeeds() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    // Seed parent merchant balance so the vault has tokens.
    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    let contract = env.current_contract_address();
    stellar.mint(&contract, &1000);

    let lbl = label(&env, "sales");
    client.register_sub_account(&merchant, &lbl);

    // Manually credit the sub-account (internal helper invoked by charge engine)
    // so we can test withdrawal.  We use Env's mock_auth to skip merchant auth.
    crate::merchant::credit_sub_account(&env, &merchant, &lbl, &token, 500).unwrap();

    let bal_before = client.get_sub_account_balance(&merchant, &lbl);
    assert_eq!(bal_before, 500);

    client.withdraw_sub_account_funds(&merchant, &lbl, &token, &200);
    assert_eq!(client.get_sub_account_balance(&merchant, &lbl), 300);
}

#[test]
fn withdraw_entire_sub_account_balance() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &500);

    let lbl = label(&env, "sales");
    client.register_sub_account(&merchant, &lbl);
    crate::merchant::credit_sub_account(&env, &merchant, &lbl, &token, 500).unwrap();

    client.withdraw_sub_account_funds(&merchant, &lbl, &token, &500);
    assert_eq!(client.get_sub_account_balance(&merchant, &lbl), 0);
}

#[test]
fn withdraw_from_nonexistent_sub_account_rejected() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "ghost");

    let result = client.try_withdraw_sub_account_funds(&merchant, &lbl, &token, &100);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

#[test]
fn withdraw_zero_amount_rejected() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "sales");
    client.register_sub_account(&merchant, &lbl);

    let result = client.try_withdraw_sub_account_funds(&merchant, &lbl, &token, &0);
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn withdraw_negative_amount_rejected() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "sales");
    client.register_sub_account(&merchant, &lbl);

    let result = client.try_withdraw_sub_account_funds(&merchant, &lbl, &token, &(-100));
    assert_eq!(result, Err(Ok(Error::InvalidAmount)));
}

#[test]
fn withdraw_exceeding_sub_account_balance_rejected() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &500);

    let lbl = label(&env, "sales");
    client.register_sub_account(&merchant, &lbl);
    crate::merchant::credit_sub_account(&env, &merchant, &lbl, &token, 100).unwrap();

    let result = client.try_withdraw_sub_account_funds(&merchant, &lbl, &token, &200);
    assert_eq!(result, Err(Ok(Error::InsufficientBalance)));
}

// ── Events ───────────────────────────────────────────────────────────────────

#[test]
fn register_sub_account_emits_event() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);
    let lbl = label(&env, "sales");

    let before = env.events().all().len();
    client.register_sub_account(&merchant, &lbl);
    assert_eq!(env.events().all().len(), before + 1);
}

#[test]
fn withdraw_sub_account_funds_emits_event() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &500);

    let lbl = label(&env, "sales");
    client.register_sub_account(&merchant, &lbl);
    crate::merchant::credit_sub_account(&env, &merchant, &lbl, &token, 500).unwrap();

    let before = env.events().all().len();
    client.withdraw_sub_account_funds(&merchant, &lbl, &token, &200);
    assert_eq!(env.events().all().len(), before + 1);
}

// ── Balance Independence ─────────────────────────────────────────────────────

#[test]
fn register_does_not_affect_parent_balance() {
    let (env, client, _admin, _token) = setup();
    let merchant = create_merchant(&client, &env);

    let parent_before = client.get_merchant_balance(&merchant);
    client.register_sub_account(&merchant, &label(&env, "sales"));
    let parent_after = client.get_merchant_balance(&merchant);

    assert_eq!(parent_before, parent_after);
}

// ── Labels are per-merchant ──────────────────────────────────────────────────

#[test]
fn sub_account_labels_independent_per_merchant() {
    let (env, client, _admin, _token) = setup();
    let merchant_a = create_merchant(&client, &env);
    let merchant_b = create_merchant(&client, &env);

    client.register_sub_account(&merchant_a, &label(&env, "sales"));
    client.register_sub_account(&merchant_b, &label(&env, "sales"));

    assert_eq!(client.get_sub_account_list(&merchant_a).len(), 1);
    assert_eq!(client.get_sub_account_list(&merchant_b).len(), 1);
    // Same label does not collide between merchants.
    assert_eq!(
        client.get_sub_account_balance(&merchant_a, &label(&env, "sales")),
        0
    );
    assert_eq!(
        client.get_sub_account_balance(&merchant_b, &label(&env, "sales")),
        0
    );
}

// ── Subscription sub_account_label ───────────────────────────────────────────

#[test]
fn create_subscription_with_sub_account_label() {
    let (env, client, admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    client.register_sub_account(&merchant, &label(&env, "sales"));

    // Set a min top-up so deposit is accepted.
    client.set_min_topup(&admin, &1);
    let subscriber = Address::generate(&env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &1000,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(label(&env, "sales")),
        )
        .unwrap();

    client.deposit_funds(&sub_id, &5000, &None);
    client.charge_subscription(&sub_id, &None);

    // Sub-account has the merchant's net charge amount (1000 with 0 fee bps).
    assert_eq!(client.get_sub_account_balance(&merchant, &label(&env, "sales")), 1000);
}

#[test]
fn create_subscription_without_sub_account_label_leaves_parent_balance() {
    let (env, client, admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    client.set_min_topup(&admin, &1);
    let subscriber = Address::generate(&env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &1000,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
        )
        .unwrap();

    client.deposit_funds(&sub_id, &5000, &None);
    client.charge_subscription(&sub_id, &None);

    // No sub-account label → funds stay in parent merchant balance.
    assert_eq!(client.get_merchant_balance(&merchant), 1000);
}

#[test]
fn one_off_charge_routes_to_sub_account() {
    let (env, client, admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    client.register_sub_account(&merchant, &label(&env, "sales"));

    client.set_min_topup(&admin, &1);
    let subscriber = Address::generate(&env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &1000,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(label(&env, "sales")),
        )
        .unwrap();

    client.deposit_funds(&sub_id, &5000, &None);

    // One-off charge of 2000
    client.charge_one_off(&sub_id, &merchant, &2000, &None);

    // 2000 should be in sub-account, not parent
    assert_eq!(client.get_sub_account_balance(&merchant, &label(&env, "sales")), 2000);
    assert_eq!(client.get_merchant_balance(&merchant), 0);
}

#[test]
fn charge_to_unregistered_sub_account_fails() {
    let (env, client, admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    // Note: we do NOT register the "sales" sub-account.

    client.set_min_topup(&admin, &1);
    let subscriber = Address::generate(&env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &1000,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(label(&env, "sales")),
        )
        .unwrap();

    client.deposit_funds(&sub_id, &5000, &None);

    // The charge engine calls credit_sub_account which checks that the
    // sub-account exists — it should fail because "sales" was never registered.
    let result = client.try_charge_subscription(&sub_id, &None);
    assert_eq!(result, Err(Ok(Error::NotFound)));
}

#[test]
fn charge_subscription_to_sub_account_updates_earnings() {
    let (env, client, admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    client.register_sub_account(&merchant, &label(&env, "sales"));

    client.set_min_topup(&admin, &1);
    let subscriber = Address::generate(&env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &1000,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(label(&env, "sales")),
        )
        .unwrap();

    client.deposit_funds(&sub_id, &5000, &None);
    client.charge_subscription(&sub_id, &None);

    // Earnings (parent-level) must reflect the charge even though the
    // funds are in the sub-account balance.
    let earnings = client.get_merchant_token_earnings(&merchant, &token);
    assert_eq!(earnings.accruals.interval, 1000);
}

// ── Sub-Account Revocation ───────────────────────────────────────────────────

#[test]
fn revoked_sub_account_cannot_withdraw() {
    let (env, client, admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &1000);

    let sales_label = label(&env, "sales");
    client.register_sub_account(&merchant, &sales_label);
    crate::merchant::credit_sub_account(&env, &merchant, &sales_label, &token, 500).unwrap();

    // Verify sub-account has funds
    assert_eq!(client.get_sub_account_balance(&merchant, &sales_label), 500);

    // Enable whitelist mode and revoke the merchant
    client.set_whitelist_mode(&admin, &true);
    client.approve_merchant(&admin, &merchant);
    client.revoke_merchant(&admin, &merchant);

    // Sub-account withdrawal must now fail because parent merchant is revoked
    let result = client.try_withdraw_sub_account_funds(&merchant, &sales_label, &token, &100);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));

    // Balance should remain unchanged
    assert_eq!(client.get_sub_account_balance(&merchant, &sales_label), 500);
}

// ── Tests for new sub-account earnings aggregation queries ──────────────────

#[test]
fn get_merchant_token_earnings_with_sub_accounts_no_subs() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&merchant, &1000);
    client.deposit_funds(
        &client.create_subscription(
            &merchant,
            &merchant,
            &token,
            &100,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
        ).unwrap(),
        &1000,
        &None,
    );

    let sub_id = client.create_subscription(
        &merchant,
        &merchant,
        &token,
        &100,
        &86400,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<Symbol>,
    ).unwrap();
    client.deposit_funds(&sub_id, &1000, &None);
    client.charge_subscription(&sub_id, &None);

    // Query the new endpoint for no sub-accounts case
    let breakdown = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);

    // Should have zero sub-accounts
    assert_eq!(breakdown.sub_account_balances.len(), 0);
    // Primary balance should equal the merchant's charged amount (minus any fees)
    assert!(breakdown.primary_balance > 0);
    // Total should equal primary since there are no sub-accounts
    assert_eq!(breakdown.total_balance, breakdown.primary_balance);
    // Earnings should match
    assert_eq!(breakdown.primary_earnings.accruals.interval, 100);
}

#[test]
fn get_merchant_token_earnings_with_sub_accounts_single_sub() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &10000);

    let sales = label(&env, "sales");
    client.register_sub_account(&merchant, &sales);

    let subscriber = Address::generate(&env);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &500,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(sales.clone()),
        )
        .unwrap();

    client.deposit_funds(&sub_id, &2500, &None);
    client.charge_subscription(&sub_id, &None);

    // Query with sub-account
    let breakdown = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);

    // Should have exactly one sub-account
    assert_eq!(breakdown.sub_account_balances.len(), 1);
    assert_eq!(breakdown.sub_account_balances[0].label, sales);
    assert_eq!(breakdown.sub_account_balances[0].balance, 500);

    // Primary balance should be zero (all funds routed to sub-account)
    assert_eq!(breakdown.primary_balance, 0);

    // Total should equal sub-account balance
    assert_eq!(breakdown.total_balance, 500);

    // Earnings should reflect the charge
    assert_eq!(breakdown.primary_earnings.accruals.interval, 500);
}

#[test]
fn get_merchant_token_earnings_with_sub_accounts_multiple_subs() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &50000);

    let sales = label(&env, "sales");
    let support = label(&env, "support");
    let ops = label(&env, "ops");

    client.register_sub_account(&merchant, &sales);
    client.register_sub_account(&merchant, &support);
    client.register_sub_account(&merchant, &ops);

    // Manually credit different amounts to each
    crate::merchant::credit_sub_account(&env, &merchant, &sales, &token, 1000).unwrap();
    crate::merchant::credit_sub_account(&env, &merchant, &support, &token, 2000).unwrap();
    crate::merchant::credit_sub_account(&env, &merchant, &ops, &token, 500).unwrap();

    let breakdown = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);

    // Should have all three sub-accounts
    assert_eq!(breakdown.sub_account_balances.len(), 3);
    assert_eq!(breakdown.total_balance, 3500);
    assert_eq!(breakdown.primary_balance, 0); // No primary balance in this scenario

    // Verify each sub-account is listed
    let mut found = vec![false; 3];
    for sub in breakdown.sub_account_balances.iter() {
        if sub.label == sales && sub.balance == 1000 {
            found[0] = true;
        }
        if sub.label == support && sub.balance == 2000 {
            found[1] = true;
        }
        if sub.label == ops && sub.balance == 500 {
            found[2] = true;
        }
    }
    assert_eq!(found, vec![true, true, true]);
}

#[test]
fn get_merchant_token_earnings_with_sub_accounts_mixed_primary_and_subs() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &50000);

    let sales = label(&env, "sales");
    client.register_sub_account(&merchant, &sales);

    let subscriber1 = Address::generate(&env);
    let subscriber2 = Address::generate(&env);
    stellar.mint(&subscriber1, &10000);
    stellar.mint(&subscriber2, &10000);

    // Charge 1: routes to primary (no sub_account_label)
    let sub_id_primary = client
        .create_subscription_with_token(
            &subscriber1,
            &merchant,
            &token,
            &300,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
        )
        .unwrap();
    client.deposit_funds(&sub_id_primary, &1000, &None);
    client.charge_subscription(&sub_id_primary, &None);

    // Charge 2: routes to sub-account (with sub_account_label)
    let sub_id_sub = client
        .create_subscription_with_token(
            &subscriber2,
            &merchant,
            &token,
            &200,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(sales.clone()),
        )
        .unwrap();
    client.deposit_funds(&sub_id_sub, &1000, &None);
    client.charge_subscription(&sub_id_sub, &None);

    let breakdown = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);

    // Should have both primary and sub-account balances
    assert_eq!(breakdown.primary_balance, 300);
    assert_eq!(breakdown.sub_account_balances.len(), 1);
    assert_eq!(breakdown.sub_account_balances[0].balance, 200);

    // Total should be the sum
    assert_eq!(breakdown.total_balance, 500);

    // Earnings should reflect both charges
    assert_eq!(breakdown.primary_earnings.accruals.interval, 500);
}

#[test]
fn get_merchant_total_earnings_with_sub_accounts_single_token() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &10000);

    let sales = label(&env, "sales");
    client.register_sub_account(&merchant, &sales);

    crate::merchant::credit_sub_account(&env, &merchant, &sales, &token, 1000).unwrap();

    let all_earnings = client.get_merchant_total_earnings_with_sub_accounts(&merchant);

    // Should have exactly one entry (one token)
    assert_eq!(all_earnings.len(), 1);
    assert_eq!(all_earnings[0].token, token);
    assert_eq!(all_earnings[0].total_balance, 1000);
}

#[test]
fn get_merchant_balance_aggregated() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &10000);

    let sales = label(&env, "sales");
    let support = label(&env, "support");
    client.register_sub_account(&merchant, &sales);
    client.register_sub_account(&merchant, &support);

    crate::merchant::credit_sub_account(&env, &merchant, &sales, &token, 1000).unwrap();
    crate::merchant::credit_sub_account(&env, &merchant, &support, &token, 500).unwrap();

    // Query aggregated balance
    let total = client.get_merchant_balance_aggregated(&merchant, &token);

    // Should equal the sum of all sub-accounts
    assert_eq!(total, 1500);
}

#[test]
fn accounting_invariant_with_sub_accounts() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &50000);

    let sales = label(&env, "sales");
    client.register_sub_account(&merchant, &sales);

    let subscriber = Address::generate(&env);
    stellar.mint(&subscriber, &10000);

    let sub_id = client
        .create_subscription_with_token(
            &subscriber,
            &merchant,
            &token,
            &250,
            &86400,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &Some(sales.clone()),
        )
        .unwrap();

    client.deposit_funds(&sub_id, &1000, &None);
    client.charge_subscription(&sub_id, &None);

    let breakdown = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);

    // Verify the accounting invariant:
    // primary_earnings.accruals - withdrawals - refunds == total_balance
    let computed = breakdown.primary_earnings.accruals.interval
        + breakdown.primary_earnings.accruals.usage
        + breakdown.primary_earnings.accruals.one_off
        - breakdown.primary_earnings.withdrawals
        - breakdown.primary_earnings.refunds;

    assert_eq!(computed, breakdown.total_balance);
}

#[test]
fn sub_account_withdrawal_updates_breakdown() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &10000);

    let sales = label(&env, "sales");
    client.register_sub_account(&merchant, &sales);

    crate::merchant::credit_sub_account(&env, &merchant, &sales, &token, 1000).unwrap();

    // Before withdrawal
    let breakdown_before = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);
    assert_eq!(breakdown_before.sub_account_balances[0].balance, 1000);
    assert_eq!(breakdown_before.total_balance, 1000);

    // Withdraw from sub-account
    client.withdraw_sub_account_funds(&merchant, &sales, &token, &400);

    // After withdrawal
    let breakdown_after = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);
    assert_eq!(breakdown_after.sub_account_balances[0].balance, 600);
    assert_eq!(breakdown_after.total_balance, 600);
    // Earnings should show the withdrawal
    assert_eq!(breakdown_after.primary_earnings.withdrawals, 400);
}

#[test]
fn backward_compatibility_old_queries_unchanged() {
    let (env, client, _admin, token) = setup();
    let merchant = create_merchant(&client, &env);

    let stellar = soroban_sdk::token::StellarAssetClient::new(&env, &token);
    stellar.mint(&env.current_contract_address(), &10000);

    let sales = label(&env, "sales");
    client.register_sub_account(&merchant, &sales);

    crate::merchant::credit_sub_account(&env, &merchant, &sales, &token, 1000).unwrap();

    // Old endpoint should still return only primary balance
    let primary_only = client.get_merchant_balance_by_token(&merchant, &token);
    assert_eq!(primary_only, 0); // Primary has zero, all is in sub-account

    // New endpoint should show the breakdown
    let breakdown = client.get_merchant_token_earnings_with_sub_accounts(&merchant, &token);
    assert_eq!(breakdown.primary_balance, 0);
    assert_eq!(breakdown.total_balance, 1000);
}
