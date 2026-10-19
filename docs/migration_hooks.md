# Migration Hooks (Subscription Vault)

This document describes the migration-friendly hooks added to the contract to support
future upgrades while preserving security and minimizing risk.

## Contents

- [Schema version migration](#schema-version-migration)
- [Rollback and downgrade](#rollback-and-downgrade)
- [Export hooks](#export-hooks)
- [Control and authorization](#control-and-authorization)
- [Suggested migration flow](#suggested-migration-flow)
- [Security and limitations](#security-and-limitations)

## Goals and scope

- Provide **admin-only**, **read-only** export hooks for contract and subscription state.
- Keep exports **bounded** and **auditable** via events.
- Avoid any mechanism that could **move funds**, **corrupt state**, or **weaken auth**.

These hooks are intended for carefully managed upgrades only. They do not enable
cross-contract transfers.

## Schema version migration

### Overview

The contract stores a `DataKey::SchemaVersion` key in contract storage. Its value
must match the binary's `STORAGE_VERSION` constant (currently `5`).

- `init` writes `SchemaVersion = STORAGE_VERSION` on first call.
- `migrate(admin, dry_run)` compares the on-chain stored version against the
  binary version and either previews or applies the required upgrade steps.

### `migrate(admin, dry_run)` entrypoint

Implemented in `contracts/subscription_vault/src/lib.rs` (delegates to `admin::do_migrate`).

| Stored version | Binary version | Result |
|:---:|:---:|:---|
| `stored > binary` | — | `Err(SchemaVersionMismatch)` (9102) — downgrade rejected |
| `stored == binary` | — | Returns a no-op summary; no event emitted |
| `stored < binary`, `dry_run = true` | — | Returns a preview summary without changing storage or emitting an event |
| `stored < binary`, `dry_run = false` | — | Applies the upgrade ladder, writes the new version, emits `SchemaMigratedEvent`, and returns the summary |

The returned `SchemaMigrationSummary` reports `from_version`, `to_version`,
the number of legacy config keys to move, the subscription rewrite counts for
the v3-to-v4 and v4-to-v5 steps, whether this was a dry run, and whether the
schema version would change. A preview still requires admin authorization.
Run with `dry_run = true` first, review the summary, then call with
`dry_run = false` to apply the same planned migration.

**Auth:** Admin only. `admin.require_auth()` is called before any state is read.

**Event:** `SchemaMigratedEvent { admin, from_version, to_version, timestamp }` is
emitted on the `schema_migrated` topic only when an actual upgrade is performed.

**Atomicity:** The `SchemaVersion` key is written **after** all upgrade steps succeed.
A mid-migration panic leaves the stored version unchanged.

### Adding a future migration path

When `STORAGE_VERSION` is bumped to `N`, add a new arm to the `match (current, binary_version)`
ladder in `admin::do_migrate`:

```rust
(N - 1, N) => {
    // perform any required state-shape changes here
    current = N;
}
```

Each arm must be self-contained and must not assume any prior arm ran in the same call.

### Security properties

- **Downgrade guard:** if the on-chain version is newer than the binary,
  `migrate()` returns `SchemaVersionMismatch` (9102); the legacy
  `migrate_config_to_persistent()` path returns `SchemaMigrationDowngrade` (9101)
  for stored versions above v3. Both prevent accidental rollback corruption.
- **Idempotent:** calling `migrate` when already at the current version is a safe no-op.
- **Admin-only:** non-admin callers are rejected with `Unauthorized`.
- **No fund movement:** `migrate` may move legacy config keys and rewrite
  subscription storage records, but performs no token transfers or balance
  changes. With `dry_run = true`, it does not write storage or emit events.
- **Audit trail:** every successful upgrade emits a `SchemaMigratedEvent` with the
  admin address, version pair, and ledger timestamp.

## Rollback and downgrade

### Rollback hooks are not supported

The contract does not implement rollback hooks. Schema migrations are **forward-only**.
This is a deliberate design decision, not an oversight.

#### Rationale

Soroban's `#[contracttype]` serialization is **positional**: each field in a struct maps to
a fixed slot in the XDR encoding. A forward migration step (e.g. v3 → v4) rewrites every
`DataKey::Sub(id)` record on-chain so that the new field layout deserializes correctly for
the upgraded binary. There is no safe, general-purpose way to reverse that rewrite:

- The old field layout is gone from the binary — the v3 deserializer no longer exists in the
  v4 WASM.
- Subscription records rewritten by the upgrade step cannot be decoded by the previous binary
  without access to the old struct definition, which is not retained.
- Re-deploying the old WASM and calling `migrate()` would immediately trigger
  `SchemaVersionMismatch` (code `9102`) and be rejected without changing storage.

Attempting a rollback by force (e.g. re-uploading old WASM without calling `migrate()`) would
leave the stored `SchemaVersion` higher than the binary constant, causing every entrypoint that
reads migrated storage keys to panic on deserialization.

#### Downgrade errors

Both migration entrypoints reject unsupported stored schemas before writing migration
changes. `migrate()` returns `SchemaVersionMismatch` (code `9102`) when
`stored_version > STORAGE_VERSION`. The legacy `migrate_config_to_persistent()` endpoint
supports schemas through v3 and returns `SchemaMigrationDowngrade` (code `9101`) when the
stored version is greater than 3:

```rust
// contracts/subscription_vault/src/admin.rs — do_migrate
let stored_version = get_schema_version(env);
if stored_version > binary_version {
    return Err(Error::SchemaVersionMismatch);
}
```

| Condition | Error | Meaning |
|-----------|-------|---------|
| `stored_version > STORAGE_VERSION` in `migrate()` | `SchemaVersionMismatch` (9102) | On-chain state is newer than the deployed binary. The call returns before migration writes or event emission. |
| `stored_version > 3` in `migrate_config_to_persistent()` | `SchemaMigrationDowngrade` (9101) | The legacy config migration only supports schemas through v3 and rejects newer state. |

#### Recovery after a downgrade rejection

The stored schema version is intentionally **not reset** when a downgrade is rejected.
It records the schema of the data already on-chain; resetting the marker would not undo
any storage-layout changes and could make the deployed binary misinterpret that data.
Repeatedly calling the same older binary's migration entrypoint will therefore return
the same error. A rejected call does not emit `SchemaMigratedEvent`.

To recover:

1. Stop retrying with the older binary. Do not manually lower or delete
   `DataKey::SchemaVersion`.
2. Determine the stored schema version from the contract's on-chain storage and confirm
   which schema layouts the candidate WASM can read. The `version()` endpoint and
   `export_contract_snapshot.storage_version` report the binary's compile-time version,
   not the stored version.
3. Upgrade to a binary that supports the current on-chain schema and whose
   `STORAGE_VERSION` is at least the stored version. If the versions match, no migration
   is needed. If the binary is newer, call `migrate(admin, true)` to review the planned
   forward migration, then call `migrate(admin, false)` to apply it.
4. If no compatible binary is available, prepare and validate one that can read the
   current storage layout before upgrading. If restoring old behavior is required, do so
   with a compatible forward hotfix; do not redeploy an older binary or falsify the
   schema marker.

If the stored version is unexpectedly high, investigate the deployment and migration
history before proceeding. See [`docs/errors.md`](errors.md) for the error table.

### Hotfix strategy when a forward migration must be undone

Because rollback is not available at the contract level, the recovery path after a bad
migration is:

1. **Do not re-deploy the old binary.** It cannot read the migrated storage layout and will
   panic on any call that touches a rewritten record.

2. **Deploy a new patched binary** at a version higher than the current on-chain version.
   The patched binary must be able to read the current (migrated) storage layout.

3. **If the migration step introduced a schema bug**, the patch binary should include a new
   migration arm (e.g. `(4, _) => { fix_up_records(env); current = 5; }`) that corrects the
   on-chain data in a forward step.

4. **Use `export_contract_snapshot` and `export_subscription_summaries` before deploying a
   new version** so that if the patch itself needs to be validated, the exported state can be
   diffed against the post-patch state off-chain.

5. **Activate the emergency stop** (`enable_emergency_stop(admin)`) before deploying a
   hotfix binary to halt all financial writes while the patch is prepared. Export hooks remain
   callable during an emergency stop (they are read-only and not gated by the circuit breaker).

### Migration version history and irreversibility

| Upgrade | What was rewritten | Reversible? |
|---------|--------------------|-------------|
| v0–v1 → v2 | No data rewrite; version counter bumped | No — v1 binary cannot read v2 instance storage layout |
| v2 → v3 | `SchemaVersion` moved from instance storage to persistent storage; config keys migrated | No — v2 binary looks for config in instance storage; v3 stores it in persistent storage |
| v3 → v4 | Every `DataKey::Sub(id)` record rewritten to add `expires_at_ledger: Option<u32>` | No — v3 binary's `Subscription` struct has no `expires_at_ledger` field; deserialization panics |
| v4 → v5 | Every `DataKey::Sub(id)` record rewritten to add `sub_account_label: Option<Symbol>` | No — v4 binary's struct has no `sub_account_label` field; deserialization panics |

### Golden fixtures as a rollback safety net

Although schema rollback is unsupported, the golden regression test suite
(`contracts/subscription_vault/tests/migration_goldens.rs`) provides bit-perfect snapshot
comparison across version boundaries. Before any migration is deployed to production:

- Run `cargo test -- --ignored update_goldens` to capture the pre-migration snapshot.
- After migration, diff old and new golden fixtures to verify only the expected fields changed.
- If the diff is unexpected, abort and prepare a hotfix binary rather than attempting to
  redeploy the old one.

---

## Export hooks

The following entrypoints are implemented in `contracts/subscription_vault/src/lib.rs`:

- `export_contract_snapshot(admin)`
  - Returns `ContractSnapshot` containing `admin`, `token`, `min_topup`, `next_id`,
    `storage_version`, and a `timestamp`.
  - Emits a `migration_contract_snapshot` event.

- `export_subscription_summary(admin, subscription_id)`
  - Returns `SubscriptionSummary` for a single subscription.
  - Emits a `migration_export` event.

- `export_subscription_summaries(admin, start_id, limit)`
  - Returns a paginated list of `SubscriptionSummary` records.
  - `limit` is capped at `MAX_EXPORT_LIMIT` (currently 100) to keep responses bounded.
  - Emits a `migration_export` event that includes `start_id`, `limit`, and `exported`.

All export functions require **admin authentication** and are read-only.

## Control and authorization

- Only the stored admin address can invoke export hooks or the migrate entrypoint.
- Each export produces an event for auditability.
- Export hooks do not alter balances, subscription status, or any storage keys.

## Suggested migration flow

1. Admin calls `export_contract_snapshot` to capture config and storage version.
2. Admin iterates through subscriptions with `export_subscription_summaries` using
   pagination (for example, `start_id = 0` and `limit = 100` until done).
3. Off-chain tooling persists the exported summaries and validates:
   - counts and IDs are consistent
   - balances and statuses are as expected
4. A new contract version is deployed and imported using a controlled, external
   migration process (out of scope for this contract).
5. Admin previews with `migrate(admin, dry_run = true)`, reviews the returned
  summary, then calls `migrate(admin, dry_run = false)` to advance
  `SchemaVersion` and apply the upgrade ladder.

## Security and limitations

- Exports are **read-only** and **admin-only** to avoid weakening security.
- No funds can be moved via these hooks.
- The contract does **not** include a generic import hook; imports are intentionally
  excluded to prevent misuse and to keep the surface area minimal.
- Storage versioning is exposed as a constant (`STORAGE_VERSION = 5`) to support
  migration tooling decisions.

## Caveats

- Export pagination is based on `next_id` and will skip missing IDs.
- Event contents are meant for audit logs, not for replay-based migrations.
- Any migration must be reviewed and validated off-chain before use.

## Schema migration test coverage (issue #435)

The following tests in `contracts/subscription_vault/src/test.rs` cover the
`migrate` entrypoint:

| Test | What it verifies |
|---|---|
| `test_init_writes_schema_version` | `init` writes `SchemaVersion = 2` |
| `test_migrate_same_version_is_noop_success` | Same-version call returns `Ok`, no event |
| `test_migrate_downgrade_is_rejected` | Stored > binary → `SchemaVersionMismatch` |
| `test_migrate_non_admin_is_rejected` | Non-admin → `Unauthorized` |
| `test_migrate_forward_upgrade_writes_version_and_emits_event` | v0 → v2: version written, event emitted |
| `test_migrate_forward_from_version_1_to_2` | v1 → v2: succeeds |
| `test_migrate_is_idempotent_after_forward_upgrade` | Second call after upgrade is no-op |
| `test_migrate_does_not_affect_subscriptions` | Subscription state unchanged after migration |
| `test_migrate_event_fields_are_correct` | Event fields match admin, versions, timestamp |
| `test_migrate_downgrade_does_not_emit_event` | Rejected downgrade emits no event |

## Migration golden regression test suite

Golden regression tests are located in:
- `contracts/subscription_vault/tests/migration_goldens.rs` — Cross-version snapshot determinism harness
- `contracts/subscription_vault/tests/snapshots/migration_goldens/*.scval.hex` — Hex-encoded deterministic snapshots

## Suggested migration flow

1. Admin calls `export_contract_snapshot` to capture config and storage version.
2. Admin iterates through subscriptions with `export_subscription_summaries` using
   pagination (for example, `start_id = 0` and `limit = 100` until done).
3. Off-chain tooling persists the exported summaries and validates:
   - counts and IDs are consistent
   - balances and statuses are as expected
4. A new contract version is deployed and imported using a controlled, external
   migration process (out of scope for this contract).

### Integration with golden regression tests

The golden regression test suite provides automated validation of snapshot stability:

1. **Initial setup:** Run `cargo test -- --ignored update_goldens` to generate golden fixtures for your contract version
2. **Development:** As you make changes, `cargo test migration_goldens` validates that exports remain deterministic
3. **Pre-release:** Golden fixtures are committed to version control and serve as regression anchors
4. **Post-upgrade:** Compare old and new golden fixtures to understand snapshot format changes
5. **Rollback safety:** Golden fixtures enable bit-perfect snapshot comparison across version boundaries

## Security and limitations

- Exports are **read-only** and **admin-only** to avoid weakening security.
- No funds can be moved via these hooks.
- The contract does **not** include a generic import hook; imports are intentionally
  excluded to prevent misuse and to keep the surface area minimal.
- Storage versioning is exposed as a constant (`STORAGE_VERSION = 5`) to support
  migration tooling decisions.

## Caveats

- Export pagination is based on `next_id` and will skip missing IDs.
- Event contents are meant for audit logs, not for replay-based migrations.
- Any migration must be reviewed and validated off-chain before use.

## Migration fixture test suite

The file `contracts/subscription_vault/src/test_migration_fixtures.rs` contains
31 tests that verify migration correctness. They cover:

### Contract snapshot invariants
- `test_migration_snapshot_captures_all_config_fields` — verifies admin, token, min_topup, next_id, storage_version, timestamp are all correct after init.
- `test_migration_snapshot_next_id_increments_with_subscriptions` — confirms next_id tracks created subscriptions.
- `test_migration_snapshot_does_not_mutate_state` — repeated snapshot calls leave subscription balances and statuses unchanged.
- `test_migration_snapshot_requires_admin` — non-admin callers are rejected.

### Single-subscription export
- `test_migration_single_summary_preserves_all_fields` — all 14 fields (subscriber, merchant, token, amount, interval, balance, status, etc.) round-trip correctly.
- `test_migration_single_summary_preserves_lifetime_cap_and_charged` — cap and charged counters survive a real charge cycle.
- `test_migration_single_summary_not_found_returns_error` — missing subscription_id returns `NotFound`.
- `test_migration_single_summary_requires_admin` — non-admin rejected.

### Paginated export
- `test_migration_paginated_export_all_subscriptions` — all IDs exported in order.
- `test_migration_paginated_export_respects_limit` — `limit=3` returns exactly 3 records.
- `test_migration_paginated_export_cursor_resumes_correctly` — two pages are disjoint and contiguous.
- `test_migration_paginated_export_empty_when_no_subscriptions` — empty vault returns empty list.
- `test_migration_paginated_export_start_beyond_range_returns_empty` — cursor past next_id returns empty.
- `test_migration_paginated_export_limit_zero_returns_empty` — limit=0 returns empty.
- `test_migration_paginated_export_limit_exceeds_max_returns_error` — limit>100 returns `InvalidExportLimit`.
- `test_migration_paginated_export_requires_admin` — non-admin rejected.

### Status preservation
All seven subscription statuses are verified to export faithfully:
- `Active`, `Paused`, `Cancelled` — via live contract transitions.
- `InsufficientBalance`, `Expired` — via direct storage patch (error-returning contract calls roll back state in the test environment).

### Balance accounting invariants
- `test_migration_export_does_not_inflate_balances` — three successive export calls leave prepaid_balance and lifetime_charged unchanged.
- `test_migration_summary_balance_matches_subscription_record` — exported balance fields match `get_subscription` after two charges.
- `test_migration_full_walk_balances_sum_matches_individual_queries` — sum of balances from paginated export equals sum from direct queries.

### Role security
- `test_migration_export_does_not_change_admin` — repeated exports do not rotate or escalate the admin address.

### Lifetime cap accounting
- `test_migration_lifetime_cap_fully_exhausted_shows_cancelled` — cap = 1 charge → status Cancelled, lifetime_charged = cap.
- `test_migration_lifetime_cap_partially_charged_preserved` — partial charge tracked; subscription stays Active.

### Expiration fields
- `test_migration_active_expiring_subscription_preserves_expires_at` — expires_at is present in summary before expiry triggers.

### Partial migration simulation
- `test_migration_full_walk_covers_all_subscriptions` — paged walk over 7 subscriptions (page size 3) collects all 7 IDs.

### Emergency stop compatibility
- `test_migration_exports_work_during_emergency_stop` — all three export hooks remain callable when emergency stop is active (exports are read-only and not blocked).

## Verified security properties

The test suite explicitly confirms:

| Property | How tested |
|---|---|
| No balance inflation | Multiple export calls; balance unchanged |
| No role escalation | Admin address identical before and after exports |
| Read-only | No state mutation observable after any export call |
| Admin-only access | Non-admin callers rejected on all three hooks |
| Status fidelity | All 7 statuses preserved in export output |
| Accounting fidelity | prepaid_balance + lifetime_charged match direct storage reads |
| Emergency stop safe | Exports unblocked during emergency stop |
