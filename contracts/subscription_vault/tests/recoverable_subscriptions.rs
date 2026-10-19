#![cfg(test)]

//! Integration coverage for the recovery-surfacing and recovery-event features.
//!
//! * `list_recoverable_subscriptions` — surfaces subscriptions that are stranded
//!   in `Cancelled` status with a non-zero `prepaid_balance` (#438).
//! * `initiator_role` on `RecoveryEvent` / `SubscriberWithdrawalEvent` — lets an
//!   auditor tell the admin recovery path from the subscriber withdrawal path
//!   (#437).

extern crate alloc;

use soroban_sdk::{
    testutils::{Address as _, Events as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient as TokenAdminClient},
    Address, Env, FromVal, String as SorobanString, Symbol, Vec,
};
use subscription_vault::{
    queries::RecoverableSubscriptionsPage, Error, RecoveryEvent, RecoveryInitiatorRole,
    RecoveryReason, SubscriberWithdrawalEvent, SubscriptionStatus, SubscriptionVault,
    SubscriptionVaultClient,
};

const MIN_TOPUP: i128 = 1_000_000;
const AMOUNT: i128 = 5_000_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60;
const NOW: u64 = 1_000_000;

struct Fixture {
    env: Env,
    client: SubscriptionVaultClient<'static>,
    token: Address,
    admin: Address,
    subscriber: Address,
    merchant: Address,
    payer: Address,
}

impl Fixture {
    fn new() -> Self {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(NOW);

        let token_admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(token_admin.clone())
            .address();

        let admin = Address::generate(&env);
        let subscriber = Address::generate(&env);
        let merchant = Address::generate(&env);
        let payer = Address::generate(&env);

        let contract_id = env.register(SubscriptionVault, ());
        let client = SubscriptionVaultClient::new(&env, &contract_id);
        client.init(&token, &7u32, &admin, &MIN_TOPUP, &(7 * 24 * 60 * 60));

        // Fund both payers so deposits move real tokens.
        let token_admin_client = TokenAdminClient::new(&env, &token);
        token_admin_client.mint(&subscriber, &500_000_000);
        token_admin_client.mint(&payer, &500_000_000);

        Self {
            env,
            client,
            token,
            admin,
            subscriber,
            merchant,
            payer,
        }
    }

    fn new_subscription(&self) -> u32 {
        self.client.create_subscription(
            &self.subscriber,
            &self.merchant,
            &AMOUNT,
            &INTERVAL,
            &false,
            &None,
            &None::<u64>,
            &None::<u32>,
        )
    }

    /// Drive a subscription into the stranded state this feature exists to
    /// surface: `Cancelled`, then funds deposited onto the dead record.
    fn make_stranded(&self, sub_id: u32, amount: i128) {
        // Cancel first, so there is no cancellation escrow to pay out and the
        // record settles at `Cancelled` with a zero balance.
        self.client.cancel_subscription(&sub_id, &self.subscriber);

        // A delegated-payer deposit is not gated on `Cancelled`, so the funds
        // land on an already-cancelled record. Nothing claims them afterwards:
        // `withdraw_subscriber_funds` only pays the escrow, which does not exist.
        self.client
            .grant_delegated_payer(&self.subscriber, &self.payer, &(NOW + 1_000_000), &amount);
        self.client
            .deposit_funds_on_behalf(&sub_id, &self.payer, &amount, &None);
    }
}

/// Collect the subscription ids from a page, preserving ascending order.
fn ids_of(env: &Env, page: &RecoverableSubscriptionsPage) -> Vec<u32> {
    let mut out = Vec::new(env);
    for entry in page.subscriptions.iter() {
        out.push_back(entry.subscription_id);
    }
    out
}

fn one_id(env: &Env, id: u32) -> Vec<u32> {
    let mut out = Vec::new(env);
    out.push_back(id);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// list_recoverable_subscriptions (#438)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn empty_contract_reports_no_stranded_subscriptions() {
    let f = Fixture::new();

    let page = f.client.list_recoverable_subscriptions(&None, &10);

    assert_eq!(page.subscriptions.len(), 0);
    assert_eq!(
        page.next_cursor, None,
        "an empty page with no cursor means the scan is complete and nothing is stranded"
    );
}

#[test]
fn active_subscription_with_balance_is_not_reported() {
    let f = Fixture::new();
    let sub_id = f.new_subscription();
    f.client.deposit_funds(&sub_id, &10_000_000, &None);

    let page = f.client.list_recoverable_subscriptions(&None, &10);

    assert_eq!(
        page.subscriptions.len(),
        0,
        "an Active subscription is not stranded even with a non-zero prepaid balance"
    );
}

#[test]
fn normally_cancelled_subscription_is_not_reported() {
    let f = Fixture::new();
    let sub_id = f.new_subscription();
    f.client.deposit_funds(&sub_id, &10_000_000, &None);

    // The normal cancellation path zeroes `prepaid_balance` and escrows the
    // amount instead, so nothing is left stranded on the record.
    f.client.cancel_subscription(&sub_id, &f.subscriber);
    let sub = f.client.get_subscription(&sub_id);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
    assert_eq!(sub.prepaid_balance, 0);

    let page = f.client.list_recoverable_subscriptions(&None, &10);
    assert_eq!(page.subscriptions.len(), 0);
}

#[test]
fn cancelled_subscription_with_stranded_balance_is_reported() {
    let f = Fixture::new();
    let sub_id = f.new_subscription();

    f.make_stranded(sub_id, 2_000_000);

    let sub = f.client.get_subscription(&sub_id);
    assert_eq!(sub.status, SubscriptionStatus::Cancelled);
    assert_eq!(sub.prepaid_balance, 2_000_000);

    let page = f.client.list_recoverable_subscriptions(&None, &10);

    assert_eq!(ids_of(&f.env, &page), one_id(&f.env, sub_id));

    let entry = page
        .subscriptions
        .get(0)
        .expect("the stranded subscription must be reported");
    assert_eq!(entry.subscription_id, sub_id);
    assert_eq!(entry.subscriber, f.subscriber);
    assert_eq!(entry.merchant, f.merchant);
    assert_eq!(entry.token, f.token);
    assert_eq!(entry.prepaid_balance, 2_000_000);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn only_cancelled_subscriptions_with_balance_are_reported() {
    let f = Fixture::new();

    // Active with a balance: not stranded.
    let active = f.new_subscription();
    f.client.deposit_funds(&active, &10_000_000, &None);

    // Cancelled, balance escrowed away: not stranded.
    let escrowed = f.new_subscription();
    f.client.deposit_funds(&escrowed, &10_000_000, &None);
    f.client.cancel_subscription(&escrowed, &f.subscriber);

    // Cancelled with a balance deposited afterwards: stranded.
    let stranded = f.new_subscription();
    f.make_stranded(stranded, 2_000_000);

    // Paused with a balance: not stranded.
    let paused = f.new_subscription();
    f.client.deposit_funds(&paused, &10_000_000, &None);
    f.client.pause_subscription(&paused, &f.subscriber);

    let page = f.client.list_recoverable_subscriptions(&None, &50);
    assert_eq!(ids_of(&f.env, &page), one_id(&f.env, stranded));
}

#[test]
fn pagination_walks_every_stranded_subscription_exactly_once() {
    let f = Fixture::new();

    let mut expected = Vec::new(&f.env);
    for _ in 0..5 {
        let sub_id = f.new_subscription();
        // A funded decoy per stranded subscription keeps the scan honest about
        // filtering rather than merely stepping through consecutive ids.
        let decoy = f.new_subscription();
        f.client.deposit_funds(&decoy, &10_000_000, &None);

        f.make_stranded(sub_id, 2_000_000);
        expected.push_back(sub_id);
    }

    let mut collected = Vec::new(&f.env);
    let mut cursor = None;
    let mut pages = 0u32;
    let mut last_cursor = 0u32;

    loop {
        let page = f.client.list_recoverable_subscriptions(&cursor, &2);
        for entry in page.subscriptions.iter() {
            collected.push_back(entry.subscription_id);
        }
        pages += 1;
        assert!(pages < 50, "pagination failed to terminate");

        match page.next_cursor {
            Some(next) => {
                assert!(
                    next > last_cursor,
                    "next_cursor must strictly advance, got {next} after {last_cursor}"
                );
                last_cursor = next;
                cursor = Some(next);
            }
            None => break,
        }
    }

    assert_eq!(
        collected, expected,
        "every stranded id must be returned exactly once, in ascending order"
    );
    assert!(pages > 1, "expected more than one page at limit=2");
}

#[test]
fn cursor_past_the_end_returns_an_empty_complete_page() {
    let f = Fixture::new();
    let sub_id = f.new_subscription();
    f.make_stranded(sub_id, 2_000_000);

    let page = f.client.list_recoverable_subscriptions(&Some(9_999), &10);

    assert_eq!(page.subscriptions.len(), 0);
    assert_eq!(page.next_cursor, None);
}

#[test]
fn rejects_out_of_range_limits() {
    let f = Fixture::new();

    assert_eq!(
        f.client.try_list_recoverable_subscriptions(&None, &0),
        Err(Ok(Error::InvalidExportLimit))
    );
    assert_eq!(
        f.client.try_list_recoverable_subscriptions(&None, &101),
        Err(Ok(Error::InvalidExportLimit))
    );
    // The documented upper bound is inclusive.
    assert!(f.client
        .try_list_recoverable_subscriptions(&None, &100)
        .is_ok());
}

// ─────────────────────────────────────────────────────────────────────────────
// initiator_role on the recovery events (#437)
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn admin_stranded_fund_recovery_event_reports_admin_role() {
    let f = Fixture::new();

    // Mint surplus straight to the contract, exactly as an accidental transfer
    // would leave it: unaccounted, therefore recoverable.
    let token_admin_client = TokenAdminClient::new(&f.env, &f.token);
    token_admin_client.mint(&f.env.current_contract_address(), &5_000_000);

    let recipient = Address::generate(&f.env);
    let recovery_id = SorobanString::from_str(&f.env, "test-recovery-437");

    f.client.recover_stranded_funds(
        &f.admin,
        &f.token,
        &recipient,
        &2_500_000,
        &recovery_id,
        &RecoveryReason::UserOverpayment,
    );

    let token_client = TokenClient::new(&f.env, &f.token);
    assert_eq!(token_client.balance(&recipient), 2_500_000);

    let mut found: Option<RecoveryEvent> = None;
    for (_, topics, data) in f.env.events().all().iter() {
        let Some(first) = topics.get(0) else { continue };
        if Symbol::from_val(&f.env, &first) == Symbol::new(&f.env, "recovery") {
            found = Some(FromVal::from_val(&f.env, &data));
        }
    }

    let event = found.expect("admin recovery must emit a recovery event");
    assert_eq!(
        event.initiator_role,
        RecoveryInitiatorRole::Admin,
        "recover_stranded_funds is admin-gated and must be tagged admin-initiated"
    );
    assert_eq!(event.admin, f.admin);
    assert_eq!(event.recipient, recipient);
    assert_eq!(event.amount, 2_500_000);
}

#[test]
fn subscriber_withdrawal_event_reports_subscriber_role() {
    let f = Fixture::new();
    let sub_id = f.new_subscription();
    f.client.deposit_funds(&sub_id, &10_000_000, &None);
    f.client.cancel_subscription(&sub_id, &f.subscriber);

    // The cancellation escrow window must elapse before the subscriber can claim.
    f.env.ledger().set_timestamp(NOW + 30 * 24 * 60 * 60);
    f.client.withdraw_subscriber_funds(&sub_id, &f.subscriber);

    let mut found: Option<SubscriberWithdrawalEvent> = None;
    for (_, topics, data) in f.env.events().all().iter() {
        let Some(first) = topics.get(0) else { continue };
        if Symbol::from_val(&f.env, &first) == Symbol::new(&f.env, "sub_withdrawn") {
            found = Some(FromVal::from_val(&f.env, &data));
        }
    }

    let event = found.expect("subscriber withdrawal must emit an event");
    assert_eq!(
        event.initiator_role,
        RecoveryInitiatorRole::Subscriber,
        "withdraw_subscriber_funds is subscriber-gated and must be tagged subscriber-initiated"
    );
    assert_eq!(event.subscription_id, sub_id);
    assert_eq!(event.subscriber, f.subscriber);
    assert_eq!(event.token, f.token);
    assert_eq!(event.amount, 10_000_000);

    // The two recovery paths stay distinguishable from the payload alone.
    assert_ne!(
        RecoveryInitiatorRole::Admin,
        RecoveryInitiatorRole::Subscriber
    );
}

#[test]
fn subscriber_withdrawal_rejects_admin_caller() {
    let f = Fixture::new();
    let sub_id = f.new_subscription();
    f.client.deposit_funds(&sub_id, &10_000_000, &None);
    f.client.cancel_subscription(&sub_id, &f.subscriber);

    // `initiator_role` only carries meaning because the paths cannot overlap:
    // the admin cannot drive the subscriber-initiated path.
    assert_eq!(
        f.client
            .try_withdraw_subscriber_funds(&sub_id, &f.admin),
        Err(Ok(Error::Forbidden))
    );
}