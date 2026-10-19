//! Tests for `get_subscriptions_by_merchant` ordering support (issue #441).
//!
//! Merchant dashboards need newest-first (reverse-chronological) results. These
//! tests pin the contract-level guarantees of the `reverse` flag on both the
//! offset query and the cursor-paginated query:
//!
//! - `reverse = false` keeps the historical insertion (oldest-first) order.
//! - `reverse = true` returns the same records in newest-first order.
//! - Pagination metadata (`next_cursor` / `total`) is direction-independent and
//!   every record is returned exactly once when chaining pages in either
//!   direction.

use crate::{
    Error, Subscription, SubscriptionVault, SubscriptionVaultClient, MAX_SUBSCRIPTION_LIST_PAGE,
};
use soroban_sdk::testutils::Address as _;
use soroban_sdk::{Address, Env, Symbol, Vec};

const AMOUNT: i128 = 1_000_000;
const INTERVAL: u64 = 30 * 24 * 60 * 60;
const PAGE_SIZE: u32 = 2;

fn setup() -> (Env, SubscriptionVaultClient<'static>, Address) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register(SubscriptionVault, ());
    let client = SubscriptionVaultClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    client.init(&token, &6, &admin, &1i128, &(7 * 24 * 60 * 60));

    let merchant = Address::generate(&env);
    (env, client, merchant)
}

/// Creates `count` subscriptions for `merchant`, each with a distinct amount so
/// the returned record order can be asserted without depending on ids.
fn create_subscriptions(
    env: &Env,
    client: &SubscriptionVaultClient,
    merchant: &Address,
    count: u32,
) -> Vec<u32> {
    let subscriber = Address::generate(env);
    let mut ids = Vec::new(env);
    for index in 0..count {
        let id = client.create_subscription(
            &subscriber,
            merchant,
            &(AMOUNT + index as i128),
            &INTERVAL,
            &false,
            &None::<i128>,
            &None::<u64>,
            &None::<u32>,
            &None::<Symbol>,
            &false,
        );
        ids.push_back(id);
    }
    ids
}

/// Collects the distinct amounts of a page, which encode creation order.
fn amounts(env: &Env, page: &Vec<Subscription>) -> Vec<i128> {
    let mut out: Vec<i128> = Vec::new(env);
    for sub in page.iter() {
        out.push_back(sub.amount);
    }
    out
}

#[test]
fn offset_query_forward_order_is_oldest_first() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 5);

    let page = client.get_subscriptions_by_merchant(&merchant, &0, &3, &false);
    assert_eq!(page.len(), 3);
    let got = amounts(&env, &page);
    assert_eq!(got.get(0).unwrap(), AMOUNT);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 1);
    assert_eq!(got.get(2).unwrap(), AMOUNT + 2);

    let next = client.get_subscriptions_by_merchant(&merchant, &3, &3, &false);
    assert_eq!(next.len(), 2);
    let got = amounts(&env, &next);
    assert_eq!(got.get(0).unwrap(), AMOUNT + 3);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 4);
}

#[test]
fn offset_query_reverse_order_is_newest_first() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 5);

    let page = client.get_subscriptions_by_merchant(&merchant, &0, &3, &true);
    assert_eq!(page.len(), 3);
    let got = amounts(&env, &page);
    assert_eq!(got.get(0).unwrap(), AMOUNT + 4);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 3);
    assert_eq!(got.get(2).unwrap(), AMOUNT + 2);

    let next = client.get_subscriptions_by_merchant(&merchant, &3, &3, &true);
    assert_eq!(next.len(), 2);
    let got = amounts(&env, &next);
    assert_eq!(got.get(0).unwrap(), AMOUNT + 1);
    assert_eq!(got.get(1).unwrap(), AMOUNT);
}

#[test]
fn offset_query_reverse_equals_reversed_forward_result() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 4);

    let forward = amounts(
        &env,
        &client.get_subscriptions_by_merchant(&merchant, &0, &MAX_SUBSCRIPTION_LIST_PAGE, &false),
    );
    let reverse = amounts(
        &env,
        &client.get_subscriptions_by_merchant(&merchant, &0, &MAX_SUBSCRIPTION_LIST_PAGE, &true),
    );

    assert_eq!(forward.len(), reverse.len());
    let mut index = 0u32;
    while index < forward.len() {
        assert_eq!(
            forward.get(index).unwrap(),
            reverse.get(forward.len() - 1 - index).unwrap(),
            "reverse page must mirror the forward page"
        );
        index += 1;
    }
}

#[test]
fn offset_query_reverse_respects_index_bounds() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 3);

    // `start` past the end of the index yields an empty page in either direction.
    let past_end = client.get_subscriptions_by_merchant(&merchant, &3, &2, &true);
    assert_eq!(past_end.len(), 0);

    // A page that runs past the end is truncated, not padded.
    let truncated = client.get_subscriptions_by_merchant(&merchant, &2, &10, &true);
    assert_eq!(truncated.len(), 1);
    assert_eq!(amounts(&env, &truncated).get(0).unwrap(), AMOUNT);

    // Unknown merchants return an empty list, not an error.
    let unknown = Address::generate(&env);
    let empty = client.get_subscriptions_by_merchant(&unknown, &0, &10, &true);
    assert_eq!(empty.len(), 0);
}

#[test]
fn offset_query_reverse_still_validates_limit() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 1);

    assert_eq!(
        client.try_get_subscriptions_by_merchant(&merchant, &0, &0u32, &true),
        Err(Ok(Error::InvalidInput))
    );
    assert_eq!(
        client.try_get_subscriptions_by_merchant(
            &merchant,
            &0,
            &(MAX_SUBSCRIPTION_LIST_PAGE + 1),
            &true
        ),
        Err(Ok(Error::InvalidInput))
    );
}

#[test]
fn paginated_query_reverse_walks_newest_first_and_terminates() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 5);

    let page1 = client.list_subscriptions_by_merchant(&merchant, &None::<u32>, &PAGE_SIZE, &true);
    assert_eq!(page1.total, 5);
    assert_eq!(page1.next_cursor, Some(PAGE_SIZE));
    let got = amounts(&env, &page1.subscriptions);
    assert_eq!(got.get(0).unwrap(), AMOUNT + 4);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 3);

    let page2 =
        client.list_subscriptions_by_merchant(&merchant, &page1.next_cursor, &PAGE_SIZE, &true);
    assert_eq!(page2.total, 5);
    assert_eq!(page2.next_cursor, Some(4));
    let got = amounts(&env, &page2.subscriptions);
    assert_eq!(got.get(0).unwrap(), AMOUNT + 2);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 1);

    let page3 =
        client.list_subscriptions_by_merchant(&merchant, &page2.next_cursor, &PAGE_SIZE, &true);
    assert_eq!(page3.total, 5);
    assert_eq!(page3.next_cursor, None);
    assert_eq!(page3.subscriptions.len(), 1);
    assert_eq!(amounts(&env, &page3.subscriptions).get(0).unwrap(), AMOUNT);

    // Past the end the cursor is exhausted rather than repeated.
    let drained = client.list_subscriptions_by_merchant(&merchant, &Some(5u32), &PAGE_SIZE, &true);
    assert_eq!(drained.subscriptions.len(), 0);
    assert_eq!(drained.total, 5);
    assert_eq!(drained.next_cursor, None);
}

#[test]
fn paginated_query_forward_order_is_unchanged() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 5);

    let page1 = client.list_subscriptions_by_merchant(&merchant, &None::<u32>, &PAGE_SIZE, &false);
    assert_eq!(page1.total, 5);
    assert_eq!(page1.next_cursor, Some(PAGE_SIZE));
    let got = amounts(&env, &page1.subscriptions);
    assert_eq!(got.get(0).unwrap(), AMOUNT);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 1);

    let page2 =
        client.list_subscriptions_by_merchant(&merchant, &page1.next_cursor, &PAGE_SIZE, &false);
    let got = amounts(&env, &page2.subscriptions);
    assert_eq!(got.get(0).unwrap(), AMOUNT + 2);
    assert_eq!(got.get(1).unwrap(), AMOUNT + 3);

    let page3 =
        client.list_subscriptions_by_merchant(&merchant, &page2.next_cursor, &PAGE_SIZE, &false);
    assert_eq!(page3.next_cursor, None);
    assert_eq!(
        amounts(&env, &page3.subscriptions).get(0).unwrap(),
        AMOUNT + 4
    );
}

#[test]
fn paginated_query_reverse_covers_every_record_exactly_once() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 7);

    let mut seen: Vec<i128> = Vec::new(&env);
    let mut cursor: Option<u32> = None;
    loop {
        let page = client.list_subscriptions_by_merchant(&merchant, &cursor, &PAGE_SIZE, &true);
        for sub in page.subscriptions.iter() {
            seen.push_back(sub.amount);
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }

    assert_eq!(seen.len(), 7);
    let mut index = 0u32;
    while index < seen.len() {
        assert_eq!(seen.get(index).unwrap(), AMOUNT + 6 - index as i128);
        index += 1;
    }
}

#[test]
fn paginated_query_reverse_validates_limit() {
    let (env, client, merchant) = setup();
    create_subscriptions(&env, &client, &merchant, 1);

    assert!(matches!(
        client.try_list_subscriptions_by_merchant(&merchant, &None::<u32>, &0u32, &true),
        Err(Ok(Error::InvalidInput))
    ));
    assert!(matches!(
        client.try_list_subscriptions_by_merchant(
            &merchant,
            &None::<u32>,
            &(MAX_SUBSCRIPTION_LIST_PAGE + 1),
            &true
        ),
        Err(Ok(Error::InvalidInput))
    ));
}

#[test]
fn reverse_query_on_empty_merchant_returns_empty_page() {
    let (_env, client, merchant) = setup();

    let page = client.list_subscriptions_by_merchant(&merchant, &None::<u32>, &PAGE_SIZE, &true);
    assert_eq!(page.subscriptions.len(), 0);
    assert_eq!(page.total, 0);
    assert_eq!(page.next_cursor, None);
    assert_eq!(
        client
            .get_subscriptions_by_merchant(&merchant, &0, &PAGE_SIZE, &true)
            .len(),
        0
    );
}
