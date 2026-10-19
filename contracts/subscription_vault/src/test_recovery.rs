#![cfg(test)]

use crate::{
    Error, RecoveryReason, SubscriptionVault, SubscriptionVaultClient,
};
use soroban_sdk::testutils::{Address as _, Events as _, Ledger as _};
use soroban_sdk::{token, Address, Env, String};

extern crate alloc;
use alloc::format;


const INTERVAL: u64 = 30 * 24 * 60 * 60;

fn setup_env() -> (Env, SubscriptionVaultClient<'static>, Address, Address) {
    let env = Env::default();
    env.ledger().with_mut(|l| {
        l.timestamp = 1_000;
        l.sequence_number = 1;
    });
    env.mock_all_auths();
    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token_addr = env.register_stellar_asset_contract_v2(admin.clone()).address();
    let min_topup = 1_000_000i128;
    client.init(&token_addr, &6, &admin, &min_topup, &(7 * 24 * 60 * 60));

    (env, client, token_addr, admin)
}

#[test]
fn test_recovery_success_all_reasons() {
    let (env, client, token_addr, admin) = setup_env();
    let recipient = Address::generate(&env);
    let _token_admin = admin.clone();
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Mint 100 USDC to contract directly (stranded funds)
    token_client.mint(&client.address, &100_000_000);

    let reasons = [
        RecoveryReason::UserOverpayment,
        RecoveryReason::FailedTransfer,
        RecoveryReason::ExpiredEscrow,
        RecoveryReason::SystemCorrection,
    ];

    for (i, reason) in reasons.iter().enumerate() {
        let recovery_id = String::from_str(&env, &format!("rec_{}", i));
        let amount = 10_000_000i128;
        
        let balance_before = token::Client::new(&env, &token_addr).balance(&recipient);

        client.recover_stranded_funds(&admin, &token_addr, &recipient, &amount, &recovery_id, reason);

        let balance_after = token::Client::new(&env, &token_addr).balance(&recipient);
        assert_eq!(balance_after - balance_before, amount);

        // Check event
        let events = env.events().all();
        if events.len() > 0 {
            let last_event = events.last().unwrap();
            assert_eq!(last_event.0, client.address);
        }

        // Let's not assert raw event contents here, just that it didn't panic and balance changed
    }
}

#[test]
fn test_recovery_unauthorized() {
    let (env, client, token_addr, _admin) = setup_env();
    let recipient = Address::generate(&env);
    let fake_admin = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    token_client.mint(&client.address, &100_000_000);

    let recovery_id = String::from_str(&env, "rec_unauth");
    
    let result = client.try_recover_stranded_funds(&fake_admin, &token_addr, &recipient, &10_000_000, &recovery_id, &RecoveryReason::UserOverpayment);
    assert_eq!(result, Err(Ok(Error::Unauthorized)));
}

#[test]
fn test_recovery_amount_validation() {
    let (env, client, token_addr, admin) = setup_env();
    let recipient = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    token_client.mint(&client.address, &100_000_000);

    // Zero amount
    let rec_zero = String::from_str(&env, "rec_zero");
    let result = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &0,
        &rec_zero,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(result, Err(Ok(Error::InvalidRecoveryAmount)));

    // Negative amount
    let rec_neg = String::from_str(&env, "rec_neg");
    let result = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &-100,
        &rec_neg,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(result, Err(Ok(Error::InvalidRecoveryAmount)));

    // Overdraw is capped to the available recoverable balance.
    let rec_over = String::from_str(&env, "rec_over");
    let result = client.recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &200_000_000, // Contract only has 100M
        &rec_over,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(result.recovered_amount, 100_000_000);
    assert_eq!(result.surplus_amount, 100_000_000);
    assert_eq!(
        token::Client::new(&env, &token_addr).balance(&recipient),
        100_000_000
    );
}

/// Boundary: `recovery_amount == recoverable`, i.e. the admin sweeps the
/// *entire* unaccounted surplus in one call.
///
/// Every other recovery test uses a strict partial amount, so the suite never
/// observes the post-transfer state where `contract_balance == total_accounted`
/// (`recoverable == 0`). That boundary is worth pinning down because:
///
/// * an exact request transfers the full recoverable amount, while an
///   oversized request is capped without touching accounted funds;
/// * the transfer leaves the contract with a zero token balance, which is the
///   state a zero-balance invariant check has to tolerate rather than reject.
///
/// With no subscriptions and 100 USDC minted straight to the contract,
/// `total_accounted` is 0, so the whole 100 USDC is recoverable and a single
/// exact-amount recovery drains the contract to zero.
#[test]
fn test_recovery_exact_balance_drains_contract_to_zero() {
    let (env, client, token_addr, admin) = setup_env();
    let recipient = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);
    let balance_client = token::Client::new(&env, &token_addr);

    let stranded = 100_000_000i128; // 100 USDC, no subscriptions
    token_client.mint(&client.address, &stranded);

    // The full balance is recoverable, and nothing is accounted for.
    let before = client.get_token_reconciliation(&token_addr);
    assert_eq!(before.contract_balance, stranded);
    assert_eq!(before.total_prepaid, 0);
    assert_eq!(before.recoverable_amount, stranded);

    // Exactly the recoverable balance is accepted and zeroes the contract.
    let rec_exact = String::from_str(&env, "rec_exact_full");
    let exact = client.recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &stranded,
        &rec_exact,
        &RecoveryReason::UserOverpayment,
    );

    assert_eq!(exact.recovered_amount, stranded);
    assert_eq!(exact.surplus_amount, 0);
    assert_eq!(balance_client.balance(&client.address), 0, "full-drain recovery must leave the contract at zero balance");
    assert_eq!(balance_client.balance(&recipient), stranded, "recipient must receive the entire recovered amount");

    // Zero surplus is still a *balanced* vault, not a broken one: the
    // reconciliation identity holds with every component at zero.
    let after = client.get_token_reconciliation(&token_addr);
    assert_eq!(after.contract_balance, 0);
    assert_eq!(after.total_prepaid, 0);
    assert_eq!(after.total_merchant_liabilities, 0);
    assert_eq!(after.recoverable_amount, 0);
    assert_eq!(after.computed_total, 0);
    assert!(after.is_balanced, "an empty vault must still report is_balanced");

    // Nothing is recoverable afterwards, so another request is a no-op and
    // reports the full request as surplus.
    let rec_second = String::from_str(&env, "rec_exact_second");
    let second = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &1,
        &rec_second,
        &RecoveryReason::UserOverpayment,
    );
    assert_eq!(
        second,
        Ok(crate::RecoveryResult {
            recovered_amount: 0,
            surplus_amount: 1,
        })
    );
    assert_eq!(balance_client.balance(&client.address), 0);
    assert_eq!(balance_client.balance(&recipient), stranded);
}

/// Boundary: exact drain of the surplus *while subscriber funds are live*.
///
/// Recovering the full recoverable amount must not eat into `total_accounted`.
/// Here the vault holds 50 USDC of prepaid subscriber funds plus 20 USDC of
/// stranded funds, so the exact recoverable amount is 20 USDC — recovering it
/// in full leaves `contract_balance == total_accounted`, with the subscription
/// still fully funded and able to complete its normal lifecycle.
#[test]
fn test_recovery_exact_surplus_leaves_accounted_funds_intact() {
    let (env, client, token_addr, admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let admin_recipient = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);
    let balance_client = token::Client::new(&env, &token_addr);

    // 50 USDC of accounted subscriber funds.
    token_client.mint(&subscriber, &50_000_000);
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &10_000_000,
        &INTERVAL,
        &false,
        &None::<i128>,
        &None::<u64>,
        &None::<u32>,
        &None::<soroban_sdk::Symbol>,
        &false,
    );
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<u64>);

    // 20 USDC of unaccounted stranded funds on top.
    let surplus = 20_000_000i128;
    token_client.mint(&client.address, &surplus);

    let before = client.get_token_reconciliation(&token_addr);
    assert_eq!(before.total_prepaid, 50_000_000);
    assert_eq!(before.contract_balance, 70_000_000);
    assert_eq!(before.recoverable_amount, surplus);
    assert!(before.is_balanced);

    // A request one unit above the surplus transfers only the available amount.
    let rec_exact = String::from_str(&env, "rec_exact_surplus");
    let result = client.recover_stranded_funds(
        &admin,
        &token_addr,
        &admin_recipient,
        &(surplus + 1),
        &rec_exact,
        &RecoveryReason::UserOverpayment,
    );
    assert_eq!(result.recovered_amount, surplus);
    assert_eq!(result.surplus_amount, 1);

    // Contract now holds exactly the accounted funds — no more, no less.
    assert_eq!(balance_client.balance(&client.address), 50_000_000, "exact surplus drain must not touch accounted funds");
    assert_eq!(balance_client.balance(&admin_recipient), surplus);

    let after = client.get_token_reconciliation(&token_addr);
    assert_eq!(after.recoverable_amount, 0, "surplus must be exhausted");
    assert_eq!(after.total_prepaid, 50_000_000, "prepaid funds must be untouched");
    assert_eq!(after.contract_balance, 50_000_000);
    assert!(after.is_balanced, "balance identity must survive the exact drain");

    // No recoverable funds remain, but the subscription is still whole.
    let rec_over = String::from_str(&env, "rec_exact_surplus_over");
    let over = client.recover_stranded_funds(
        &admin,
        &token_addr,
        &admin_recipient,
        &1,
        &rec_over,
        &RecoveryReason::UserOverpayment,
    );
    assert_eq!(
        over,
        crate::RecoveryResult {
            recovered_amount: 0,
            surplus_amount: 1,
        }
    );

    // The subscription is still fully funded, so it charges normally after the
    // exact drain: 10 USDC moves from prepaid into merchant liabilities and the
    // vault still reports a balanced, zero-surplus reconciliation.
    env.ledger().with_mut(|l| l.timestamp = INTERVAL + 1001);
    client.charge_subscription(&sub_id, &None::<soroban_sdk::BytesN<32>>);
    let charged = client.get_token_reconciliation(&token_addr);
    assert_eq!(charged.total_prepaid, 40_000_000);
    assert_eq!(charged.total_merchant_liabilities, 10_000_000);
    assert_eq!(charged.recoverable_amount, 0);
    assert_eq!(charged.contract_balance, 50_000_000);
    assert!(charged.is_balanced);

    // The accrued funds stay with the merchant, and the subscriber is refunded
    // the remaining 40 USDC on cancellation. Every unit is accounted for, so the
    // contract ends at exactly zero.
    client.withdraw_merchant_funds(&merchant, &10_000_000);
    client.cancel_subscription(&sub_id, &subscriber);
    client.withdraw_subscriber_funds(&sub_id, &subscriber);

    assert_eq!(balance_client.balance(&subscriber), 40_000_000, "subscriber must receive the unspent prepaid balance");
    assert_eq!(balance_client.balance(&merchant), 10_000_000, "merchant must receive the charged amount");
    assert_eq!(balance_client.balance(&admin_recipient), surplus, "recovery must not be re-taken from the subscriber's funds");
    assert_eq!(balance_client.balance(&client.address), 0, "no tokens may remain after all accounted funds are paid out");

    let final_state = client.get_token_reconciliation(&token_addr);
    assert_eq!(final_state.contract_balance, 0);
    assert_eq!(final_state.total_prepaid, 0);
    assert_eq!(final_state.total_merchant_liabilities, 0);
    assert_eq!(final_state.recoverable_amount, 0);
    assert!(final_state.is_balanced);
}

#[test]
fn test_recovery_replay_protection() {
    let (env, client, token_addr, admin) = setup_env();
    let recipient = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    token_client.mint(&client.address, &100_000_000);

    let recovery_id = String::from_str(&env, "rec_replay");

    // First call succeeds
    client.recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &10_000_000,
        &recovery_id,
        &RecoveryReason::UserOverpayment
    );

    // Second call with same ID fails
    let result = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &10_000_000,
        &recovery_id,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(result, Err(Ok(Error::Replay)));
}

#[test]
fn test_state_consistency() {
    let (env, client, token_addr, admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);
    let recipient = Address::generate(&env);

    // 1. Setup subscription and deposit
    token_client.mint(&subscriber, &50_000_000);
    
    let sub_id = client.create_subscription(&subscriber, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);
    
    // Total accounted and contract balance are both 50M. A request cannot
    // recover those funds and reports the request as surplus.
    let rec_id = String::from_str(&env, "rec_steal");
    let result = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &1,
        &rec_id,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(
        result,
        Ok(crate::RecoveryResult {
            recovered_amount: 0,
            surplus_amount: 1,
        })
    );

    // 2. Stranded funds arrive (20M)
    token_client.mint(&client.address, &20_000_000);

    // 3. Try to recover 1 unit more than the available surplus - it is capped.
    let rec_id2 = String::from_str(&env, "rec_over");
    let result2 = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &20_000_001,
        &rec_id2,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(
        result2,
        Ok(crate::RecoveryResult {
            recovered_amount: 20_000_000,
            surplus_amount: 1,
        })
    );

    // 4. Repeating the request after the surplus was exhausted is a no-op.
    let rec_id3 = String::from_str(&env, "rec_exact");
    let result3 = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &recipient,
        &20_000_000,
        &rec_id3,
        &RecoveryReason::UserOverpayment
    );
    assert_eq!(
        result3,
        Ok(crate::RecoveryResult {
            recovered_amount: 0,
            surplus_amount: 20_000_000,
        })
    );

    // 5. Normal operation still works (withdraw)
    client.cancel_subscription(&sub_id, &subscriber);
    client.withdraw_subscriber_funds(&sub_id, &subscriber);

    let sub_balance = token::Client::new(&env, &token_addr).balance(&subscriber);
    assert_eq!(sub_balance, 50_000_000); // Got refund back
}

/// Locks in the behaviour documented in `docs/recovery.md` →
/// "Recovery after subscription cancellation" (#245).
///
/// The documented rule is that `recover_stranded_funds` is **not** a recovery
/// path for a cancelled subscription, and that the exclusion is
/// *accounting-based* (recoverable = contract_balance - total_accounted) rather
/// than status-based. This test proves both halves:
///
/// 1. While a cancelled subscription's refund is still escrowed (and therefore
///    still inside `total_accounted`), the admin cannot recover it.
/// 2. Cancellation by itself does not change the recoverable surplus, so it
///    never creates admin-recoverable funds.
#[test]
fn test_recovery_rejected_for_cancelled_subscription_funds() {
    let (env, client, token_addr, admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let admin_recipient = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);
    let balance_client = token::Client::new(&env, &token_addr);

    // Prepay 50 USDC on an active subscription.
    token_client.mint(&subscriber, &50_000_000);
    let sub_id = client.create_subscription(
        &subscriber,
        &merchant,
        &10_000_000,
        &INTERVAL,
        &false,
        &None,
        &None::<u64> & None::<u32>,
    );
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Baseline: no unaccounted surplus, so nothing is recoverable.
    let before = client.get_token_reconciliation(&token_addr);
    assert_eq!(before.recoverable_amount, 0i128);

    // Cancel. The refund moves into the time-locked cancellation escrow and
    // stays inside total_accounted, so the surplus is still exactly zero.
    client.cancel_subscription(&sub_id, &subscriber);
    let after_cancel = client.get_token_reconciliation(&token_addr);
    assert_eq!(after_cancel.recoverable_amount, 0i128);

    // The admin must not be able to sweep the escrowed refund.
    let rec_id = String::from_str(&env, "rec_cancelled_escrow");
    let attempt = client.try_recover_stranded_funds(
        &admin,
        &token_addr,
        &admin_recipient,
        &50_000_000,
        &rec_id,
        &RecoveryReason::ExpiredEscrow,
    );
    assert_eq!(
        attempt,
        Ok(crate::RecoveryResult {
            recovered_amount: 0,
            surplus_amount: 50_000_000,
        })
    );
    assert_eq!(balance_client.balance(&admin_recipient), 0i128);

    // After the subscriber withdraws, the refund has left the contract and the
    // surplus is *still* zero — the subscriber, not the admin, is paid.
    client.withdraw_subscriber_funds(&sub_id, &subscriber);
    let after_withdraw = client.get_token_reconciliation(&token_addr);
    assert_eq!(after_withdraw.recoverable_amount, 0i128);
    assert_eq!(balance_client.balance(&subscriber), 50_000_000);
    assert_eq!(balance_client.balance(&admin_recipient), 0i128);
}

// ── Reconciliation Query Tests ───────────────────────────────────────────────

use crate::{
    PrepaidQueryRequest, PrepaidQueryResult, ReconciliationProof, ReconciliationSummaryPage,
    TokenLiabilities,
};

#[test]
fn test_get_token_reconciliation_empty_contract() {
    let (env, client, token_addr, _admin) = setup_env();

    // Empty contract - no subscriptions, no funds
    let reconciliation = client.get_token_reconciliation(&token_addr);

    assert_eq!(reconciliation.token, token_addr);
    assert_eq!(reconciliation.total_prepaid, 0);
    assert_eq!(reconciliation.total_merchant_liabilities, 0);
    assert_eq!(reconciliation.recoverable_amount, 0);
    assert_eq!(reconciliation.contract_balance, 0);
    assert_eq!(reconciliation.computed_total, 0);
    assert!(reconciliation.is_balanced);
    assert_eq!(reconciliation.normalized_prepaid, 0);
    assert_eq!(reconciliation.normalized_merchant_liab, 0);
    assert_eq!(reconciliation.normalized_recoverable, 0);
    assert_eq!(reconciliation.normalized_contract_balance, 0);
    assert_eq!(reconciliation.normalized_computed_total, 0);
}

#[test]
fn test_get_token_reconciliation_with_prepaid() {
    let (env, client, token_addr, _admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Setup: Create subscription and deposit funds
    token_client.mint(&subscriber, &50_000_000);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Get reconciliation
    let reconciliation = client.get_token_reconciliation(&token_addr);

    assert_eq!(reconciliation.token, token_addr);
    assert_eq!(reconciliation.total_prepaid, 50_000_000);
    assert_eq!(reconciliation.contract_balance, 50_000_000);
    // No charges yet, so merchant liabilities should be 0
    assert_eq!(reconciliation.total_merchant_liabilities, 0);
    assert_eq!(reconciliation.recoverable_amount, 0);
    assert_eq!(reconciliation.computed_total, 50_000_000);
    assert!(reconciliation.is_balanced);
    // 6 decimals to 9 decimals scales by 1000
    assert_eq!(reconciliation.normalized_prepaid, 50_000_000_000);
    assert_eq!(reconciliation.normalized_contract_balance, 50_000_000_000);
    assert_eq!(reconciliation.normalized_merchant_liab, 0);
    assert_eq!(reconciliation.normalized_recoverable, 0);
    assert_eq!(reconciliation.normalized_computed_total, 50_000_000_000);
}

#[test]
fn test_get_token_reconciliation_after_charge() {
    let (env, client, token_addr, admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Setup: Create subscription with funds and charge it
    token_client.mint(&subscriber, &50_000_000);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Charge the subscription
    env.ledger().with_mut(|l| l.timestamp = INTERVAL + 1001);
    client.charge_subscription(&sub_id, &None::<soroban_sdk::BytesN<32>>);

    // Get reconciliation
    let reconciliation = client.get_token_reconciliation(&token_addr);

    assert_eq!(reconciliation.total_prepaid, 40_000_000); // 50M - 10M charged
    assert_eq!(reconciliation.contract_balance, 50_000_000); // Total still 50M
    assert_eq!(reconciliation.total_merchant_liabilities, 10_000_000); // 10M accrued
    assert_eq!(reconciliation.recoverable_amount, 0);
    assert_eq!(reconciliation.computed_total, 50_000_000);
    assert!(reconciliation.is_balanced);
    // 6 decimals to 9 decimals scales by 1000
    assert_eq!(reconciliation.normalized_prepaid, 40_000_000_000);
    assert_eq!(reconciliation.normalized_contract_balance, 50_000_000_000);
    assert_eq!(reconciliation.normalized_merchant_liab, 10_000_000_000);
    assert_eq!(reconciliation.normalized_recoverable, 0);
    assert_eq!(reconciliation.normalized_computed_total, 50_000_000_000);
}

#[test]
fn test_get_token_reconciliation_with_recoverable() {
    let (env, client, token_addr, _admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Setup: Create subscription with funds
    token_client.mint(&subscriber, &50_000_000);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Mint directly to contract (stranded funds)
    token_client.mint(&client.address, &25_000_000);

    // Get reconciliation
    let reconciliation = client.get_token_reconciliation(&token_addr);

    assert_eq!(reconciliation.total_prepaid, 50_000_000);
    assert_eq!(reconciliation.contract_balance, 75_000_000);
    assert_eq!(reconciliation.total_merchant_liabilities, 0);
    assert_eq!(reconciliation.recoverable_amount, 25_000_000);
    assert_eq!(reconciliation.computed_total, 75_000_000);
    assert!(reconciliation.is_balanced);
    // 6 decimals to 9 decimals scales by 1000
    assert_eq!(reconciliation.normalized_prepaid, 50_000_000_000);
    assert_eq!(reconciliation.normalized_contract_balance, 75_000_000_000);
    assert_eq!(reconciliation.normalized_merchant_liab, 0);
    assert_eq!(reconciliation.normalized_recoverable, 25_000_000_000);
    assert_eq!(reconciliation.normalized_computed_total, 75_000_000_000);
}

#[test]
fn test_get_contract_reconciliation_summary() {
    let (env, client, token_addr, _admin) = setup_env();

    // Get summary for the single token
    let summary = client.get_recon_summary(&0, &50);

    assert_eq!(summary.token_summaries.len(), 1);
    assert!(summary.next_token_index.is_none());

    let token_summary = summary.token_summaries.get(0).unwrap();
    assert_eq!(token_summary.token, token_addr);
    assert_eq!(token_summary.normalized_prepaid, 0);
    assert_eq!(token_summary.normalized_merchant_liab, 0);
}

#[test]
fn test_get_contract_reconciliation_summary_pagination() {
    let (env, client, token1, admin) = setup_env();

    // Add a second token
    let token2 = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.add_accepted_token(&admin, &token2, &6);

    // Get first page with limit 1
    let summary_page1 = client.get_recon_summary(&0, &1);
    assert_eq!(summary_page1.token_summaries.len(), 1);
    assert!(summary_page1.next_token_index.is_some());
    assert_eq!(summary_page1.next_token_index.unwrap(), 1);

    // Get second page
    let summary_page2 = client.get_recon_summary(&1, &1);
    assert_eq!(summary_page2.token_summaries.len(), 1);
    assert!(summary_page2.next_token_index.is_none());
}

#[test]
fn test_generate_reconciliation_proof() {
    let (env, client, token_addr, _admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Setup: Create subscription with funds
    token_client.mint(&subscriber, &50_000_000);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Mint stranded funds
    token_client.mint(&client.address, &10_000_000);

    // Generate proof
    let proof = client.generate_reconciliation_proof(&token_addr);

    assert_eq!(proof.token, token_addr);
    assert_eq!(proof.contract_balance, 60_000_000);
    assert_eq!(proof.total_prepaid, 50_000_000);
    assert_eq!(proof.total_merchant_liabilities, 0);
    assert_eq!(proof.computed_recoverable, 10_000_000);
    assert_eq!(proof.subscription_count, 1);
    assert!(proof.is_valid);
    assert!(proof.timestamp > 0);
    assert!(proof.ledger_sequence > 0);
}

#[test]
fn test_generate_reconciliation_proof_empty() {
    let (env, client, token_addr, _admin) = setup_env();

    let proof = client.generate_reconciliation_proof(&token_addr);

    assert_eq!(proof.token, token_addr);
    assert_eq!(proof.contract_balance, 0);
    assert_eq!(proof.total_prepaid, 0);
    assert_eq!(proof.total_merchant_liabilities, 0);
    assert_eq!(proof.computed_recoverable, 0);
    assert_eq!(proof.subscription_count, 0);
    assert!(proof.is_valid);
}

#[test]
fn test_query_prepaid_balances_paginated() {
    let (env, client, token_addr, _admin) = setup_env();
    let subscriber1 = Address::generate(&env);
    let subscriber2 = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Setup: Create two subscriptions with funds
    token_client.mint(&subscriber1, &30_000_000);
    token_client.mint(&subscriber2, &20_000_000);

    let sub1 =
        client.create_subscription(&subscriber1, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    let sub2 =
        client.create_subscription(&subscriber2, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);

    client.deposit_funds(&sub1, &30_000_000i128, &None::<soroban_sdk::BytesN<32>>);
    client.deposit_funds(&sub2, &20_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Query first page
    let request = PrepaidQueryRequest {
        token: token_addr.clone(),
        start_subscription_id: 0,
        scan_limit: 1, // Small limit to force pagination
    };

    let result1 = client.query_prepaid_balances_paginated(&request);
    assert_eq!(result1.token, token_addr);
    assert_eq!(result1.partial_total, 30_000_000); // First subscription
    assert_eq!(result1.subscriptions_count, 1);
    assert!(result1.has_more);
    assert!(result1.next_start_id.is_some());

    // Query second page
    let request2 = PrepaidQueryRequest {
        token: token_addr.clone(),
        start_subscription_id: result1.next_start_id.unwrap(),
        scan_limit: 1,
    };

    let result2 = client.query_prepaid_balances_paginated(&request2);
    assert_eq!(result2.partial_total, 20_000_000); // Second subscription
    assert_eq!(result2.subscriptions_count, 1);
    assert!(!result2.has_more);
    assert!(result2.next_start_id.is_none());
}

#[test]
fn test_query_prepaid_balances_paginated_empty() {
    let (env, client, token_addr, _admin) = setup_env();

    let request = PrepaidQueryRequest {
        token: token_addr.clone(),
        start_subscription_id: 0,
        scan_limit: 100,
    };

    let result = client.query_prepaid_balances_paginated(&request);

    assert_eq!(result.token, token_addr);
    assert_eq!(result.partial_total, 0);
    assert_eq!(result.subscriptions_count, 0);
    assert!(!result.has_more);
    assert!(result.next_start_id.is_none());
}

#[test]
fn test_query_prepaid_balances_paginated_wrong_token() {
    let (env, client, token_addr, admin) = setup_env();
    let subscriber = Address::generate(&env);
    let merchant = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // Setup with token1
    token_client.mint(&subscriber, &50_000_000);
    let sub_id =
        client.create_subscription(&subscriber, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    client.deposit_funds(&sub_id, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // Query with a different token
    let token2 = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.add_accepted_token(&admin, &token2, &6);

    let request = PrepaidQueryRequest {
        token: token2.clone(),
        start_subscription_id: 0,
        scan_limit: 100,
    };

    let result = client.query_prepaid_balances_paginated(&request);

    assert_eq!(result.token, token2);
    assert_eq!(result.partial_total, 0); // No subscriptions with token2
    assert_eq!(result.subscriptions_count, 0);
}

#[test]
fn test_query_prepaid_balances_paginated_scan_limit_capped() {
    let (env, client, token_addr, _admin) = setup_env();

    // Request more than MAX_PREPAID_SCAN_DEPTH (500)
    let request = PrepaidQueryRequest {
        token: token_addr.clone(),
        start_subscription_id: 0,
        scan_limit: 1000, // Will be capped to 500
    };

    let result = client.query_prepaid_balances_paginated(&request);

    // Should work but with capped limit
    assert_eq!(result.token, token_addr);
    // Empty contract, so no results
    assert_eq!(result.partial_total, 0);
}

#[test]
fn test_full_reconciliation_workflow() {
    let (env, client, token_addr, admin) = setup_env();
    let subscriber1 = Address::generate(&env);
    let subscriber2 = Address::generate(&env);
    let merchant = Address::generate(&env);
    let recipient = Address::generate(&env);
    let token_client = token::StellarAssetClient::new(&env, &token_addr);

    // 1. Setup multiple subscriptions
    token_client.mint(&subscriber1, &100_000_000);
    token_client.mint(&subscriber2, &50_000_000);

    let sub1 =
        client.create_subscription(&subscriber1, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);
    let sub2 =
        client.create_subscription(&subscriber2, &merchant, &10_000_000, &INTERVAL, &false, &None, &None::<u64>&None::<u32>,
);

    client.deposit_funds(&sub1, &100_000_000i128, &None::<soroban_sdk::BytesN<32>>);
    client.deposit_funds(&sub2, &50_000_000i128, &None::<soroban_sdk::BytesN<32>>);

    // 2. Add stranded funds
    token_client.mint(&client.address, &25_000_000);

    // 3. Charge one subscription
    env.ledger().with_mut(|l| l.timestamp = INTERVAL + 1001);
    client.charge_subscription(&sub1, &None::<soroban_sdk::BytesN<32>>);

    // 4. Verify reconciliation
    let reconciliation = client.get_token_reconciliation(&token_addr);

    assert_eq!(reconciliation.total_prepaid, 140_000_000); // 90M + 50M remaining
    assert_eq!(reconciliation.total_merchant_liabilities, 10_000_000); // 1 charge
    assert_eq!(reconciliation.contract_balance, 175_000_000); // 150M + 25M stranded
    assert_eq!(reconciliation.recoverable_amount, 25_000_000);
    assert!(reconciliation.is_balanced);

    // 5. Merchant withdraws
    client.withdraw_merchant_funds(&merchant, &10_000_000);

    // 6. Verify updated reconciliation
    let reconciliation_after = client.get_token_reconciliation(&token_addr);

    assert_eq!(reconciliation_after.total_merchant_liabilities, 0); // 0 remaining after withdrawal
    assert_eq!(reconciliation_after.contract_balance, 165_000_000); // 10M withdrawn
    assert!(reconciliation_after.is_balanced);

    // 7. Generate proof
    let proof = client.generate_reconciliation_proof(&token_addr);
    assert!(proof.is_valid);
    assert_eq!(proof.subscription_count, 2);

    // 8. Paginated prepaid query
    let request = PrepaidQueryRequest {
        token: token_addr.clone(),
        start_subscription_id: 0,
        scan_limit: 500,
    };
    let prepaid_result = client.query_prepaid_balances_paginated(&request);
    assert_eq!(prepaid_result.partial_total, 140_000_000);
    assert_eq!(prepaid_result.subscriptions_count, 2);
}
