# Merchant Subscription Views

Query subscriptions associated with a merchant address on the `subscription_vault` contract.

---

## Functions

### `get_subscriptions_by_merchant`

Returns a paginated list of subscriptions for a given merchant.

```rust
pub fn get_subscriptions_by_merchant(
    env: Env,
    merchant: Address,
    start: u32,
    limit: u32,
    reverse: bool,
) -> Result<Vec<Subscription>, Error>
```

| Parameter  | Type      | Description                                    |
|------------|-----------|------------------------------------------------|
| `merchant` | `Address` | Merchant address to query                      |
| `start`    | `u32`     | 0-based offset into the merchant's id list   |
| `limit`    | `u32`     | Page size: must be `1`–`100` inclusive         |
| `reverse`  | `bool`    | `false` = oldest first (insertion order); `true` = newest first (reverse-chronological) |

**Returns:** `Ok(Vec<Subscription>)` in stable index order (subscription creation / insertion order) when `reverse` is `false`, or in reverse-chronological (newest-first) order when `reverse` is `true`. Returns `Err(Error::InvalidInput)` if `limit` is `0` or greater than `100`. Returns an empty vec if `start` is past the end of the id list.

**Ordering:** `reverse = true` walks the merchant's index from its end instead of inverting it, so the newest subscriptions come first at the same storage cost as a forward page. `start` always counts from the beginning of the selected ordering (offset `0` is the oldest subscription when `reverse` is `false` and the newest when `reverse` is `true`).

**Index assumption:** Offsets are into the stored `MerchantSubs` id vector. If an id ever had no subscription record (abnormal), that slot is skipped in the returned vec; use `get_merchant_subscription_count` for the list length when paging, not `result.len()` alone.

#### Usage example (Soroban CLI)

```bash
soroban contract invoke \
  --id <CONTRACT_ID> \
  --source <IDENTITY> \
  -- get_subscriptions_by_merchant \
  --merchant <MERCHANT_ADDRESS> \
  --start 0 \
  --limit 10 \
  --reverse true
```

---

### `list_subscriptions_by_merchant`

Cursor-based variant of `get_subscriptions_by_merchant`. Prefer this for
infinite-scroll or dashboard pagination: the returned `next_cursor` can be
replayed directly without recomputing offsets.

```rust
pub fn list_subscriptions_by_merchant(
    env: Env,
    merchant: Address,
    cursor: Option<u32>,
    limit: u32,
    reverse: bool,
) -> Result<SubscriptionsMerchantPage, Error>
```

| Parameter  | Type             | Description                                             |
|------------|------------------|---------------------------------------------------------|
| `merchant` | `Address`        | Merchant address to query                               |
| `cursor`   | `Option<u32>`    | Offset returned as `next_cursor` by the previous page; `None` starts at the beginning |
| `limit`    | `u32`            | Page size: must be `1`–`100` inclusive                  |
| `reverse`  | `bool`           | `false` = oldest first; `true` = newest first           |

**Returns:** `SubscriptionsMerchantPage { subscriptions, next_cursor, total }`.
`next_cursor` is `Some(offset)` while more results remain and `None` once the
enumeration is complete or the cursor-depth limit is reached. Traversal is
capped at `MAX_MERCHANT_CURSOR_DEPTH` (1,000) index entries across all pages. A
page that reaches the cap has no `next_cursor`; a request at or beyond the cap
returns an empty terminal page (a tombstone). `total` is the merchant's full
index length and is the same in both directions, so it may exceed the capped
number of entries available through this cursor endpoint.

**Cursors are direction-specific.** A cursor is an offset into the ordering
selected by `reverse`, so page with the same `reverse` value that produced the
cursor. Mixing directions with a cursor produces an undefined page.

```bash
soroban contract invoke \
  --id <CONTRACT_ID> \
  --source <IDENTITY> \
  -- list_subscriptions_by_merchant \
  --merchant <MERCHANT_ADDRESS> \
  --limit 10 \
  --reverse true
```

---

### `get_merchant_subscription_count`

Returns the total number of subscriptions for a merchant. Useful for pagination metadata and dashboard summaries.

```rust
pub fn get_merchant_subscription_count(env: Env, merchant: Address) -> u32
```

| Parameter  | Type      | Description               |
|------------|-----------|---------------------------|
| `merchant` | `Address` | Merchant address to query |

**Returns:** `u32` — count of subscriptions belonging to the merchant.

---

## Pagination

Use `start` and `limit` to page through results:

```
Page 1: start=0,  limit=10  → subscriptions 0–9
Page 2: start=10, limit=10  → subscriptions 10–19
Page 3: start=20, limit=10  → subscriptions 20–29 (or fewer if end of list)
```

Combine with `get_merchant_subscription_count` to calculate total pages:

```
total_pages = ceil(count / limit)
```

---

## Performance notes

- **Index storage:** Each merchant has a `Vec<u32>` of subscription IDs stored under `DataKey::MerchantSubs(merchant)`. The index is maintained automatically when subscriptions are created.
- **Ordering:** Results are in chronological (insertion) order by default; pass `reverse = true` for reverse-chronological (newest-first) results. Reverse pages cost the same as forward pages because the index is walked from its end rather than inverted.
- **Cost:** Reading is proportional to the `limit` value, not the total number of merchant subscriptions (the ID list is loaded, but only the requested slice of subscriptions is fetched from storage).
- **Best practice:** Use small `limit` values (10–50) for UI pagination to keep transaction budgets low.

---

## Integration patterns

### Merchant dashboard

1. Call `get_merchant_subscription_count(merchant)` on page load for pagination metadata.
2. Call `get_subscriptions_by_merchant(merchant, page * pageSize, pageSize, reverse)` for each
   page — pass `reverse = true` to show the newest subscriptions first.
3. Alternatively, drive the list with `list_subscriptions_by_merchant(merchant, cursor, pageSize, reverse)`
   and reuse the returned `next_cursor`.
4. Display subscription details, filter client-side by status if needed.

### Reporting / export

For merchants with large subscription counts, iterate through all pages:

```
offset = 0
while offset < count:
    page = get_subscriptions_by_merchant(merchant, offset, 50, true)  # newest first
    process(page)
    offset += 50
```
