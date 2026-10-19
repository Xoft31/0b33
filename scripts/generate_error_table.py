#!/usr/bin/env python3
"""generate_error_table.py — Generate the Error-variant cross-reference table for docs/errors.md.

Usage
-----
    python scripts/generate_error_table.py [--check]

Without ``--check`` the script rewrites the "## Entrypoint Cross-Reference" section
(and the "## Undocumented Variants" sentinel) inside ``docs/errors.md`` in-place.

With ``--check`` the script exits with a non-zero status when the file would
change (used by CI to fail the workflow when the table is stale).

Independently of ``--check``, the script exits non-zero when any ``Error``
variant present in ``types.rs`` has no entry in the ``REMEDIATION`` map.  A new
error variant therefore cannot be merged undocumented: CI fails until the
variant has documented handling, and the generated table stops showing the
"⚠ No remediation documented" placeholder.

Exit codes:
    0 — table up to date and every variant documented
    1 — stale table, and/or undocumented variants present
    2 — expected path missing, or no variants could be parsed

Algorithm
---------
1.  Parse ``contracts/subscription_vault/src/types.rs`` to extract every
    ``Error`` variant with its numeric code and doc comment.
2.  Grep every ``*.rs`` source file under ``contracts/`` for occurrences of
    ``Error::<Variant>`` to build a list of emitting entrypoints per variant.
3.  Look up the remediation and related event from a curated static mapping
    (kept in this script so it is the single source of truth for prose that
    cannot be derived mechanically).
4.  Emit a Markdown table and splice it into ``docs/errors.md`` between the
    sentinel comments::

        <!-- GENERATED:entrypoint-table:start -->
        …table…
        <!-- GENERATED:entrypoint-table:end -->

Security notes
--------------
* The script reads only files inside the repository root it is passed (or the
  auto-detected root relative to its own location).  It never makes network
  requests and writes only the single ``docs/errors.md`` target.
* Path traversal is prevented by ``Path.resolve()`` comparisons before any
  file read.
* Input parsed from source files is treated as untrusted: only a narrow
  allowlist regex is used to extract variant names and numeric codes.
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path
from typing import NamedTuple

# ---------------------------------------------------------------------------
# Repository layout
# ---------------------------------------------------------------------------

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent

TYPES_RS = REPO_ROOT / "contracts" / "subscription_vault" / "src" / "types.rs"
CONTRACTS_SRC = REPO_ROOT / "contracts" / "subscription_vault" / "src"
ERRORS_MD = REPO_ROOT / "docs" / "errors.md"

START_SENTINEL = "<!-- GENERATED:entrypoint-table:start -->"
END_SENTINEL = "<!-- GENERATED:entrypoint-table:end -->"

# ---------------------------------------------------------------------------
# Static remediation / event mapping
# (Variant → (recovery_action, related_event, deprecated))
# ---------------------------------------------------------------------------

# fmt: off
REMEDIATION: dict[str, tuple[str, str, bool]] = {
    # Auth
    "Unauthorized":                   ("Rebuild request with correct signer; do not retry unchanged.", "AdminRotatedEvent (if admin changed)", False),
    "Forbidden":                      ("Surface permission error; caller authenticated but not authorised for resource.", "—", False),
    "SubscriberBlocklisted":          ("Escalate to admin/support flow; stop retrying.", "BlocklistAddedEvent", False),
    "SelfRotation":                   ("Fix request payload — new_admin must differ from current_admin.", "—", False),
    "NonceAlreadyUsed":               ("Re-fetch nonce via get_admin_nonce / get_operator_nonce, then retry.", "NonceConsumedEvent", False),
    "BatchTooLarge":                  ("Reduce batch size to ≤ BATCH_MAX_SIZE entries and retry.", "—", False),
    # Not found
    "NotFound":                       ("Verify identifiers before retrying.", "—", False),
    "NotInitialized":                 ("Admin must call init before any other operation.", "—", False),
    # Invalid args
    "InvalidAmount":                  ("Fix input; amount must be > 0.", "—", False),
    "InvalidInput":                   ("Fix request parameters.", "—", False),
    "InvalidRecoveryAmount":          ("Fix amount; must be > 0.", "—", False),
    "InvalidNewAdmin":                ("Fix payload; new_admin must not equal contract address.", "—", False),
    "MetadataKeyTooLong":             ("Trim key to ≤ MAX_METADATA_KEY_LENGTH bytes and retry.", "—", False),
    "MetadataValueTooLong":           ("Trim value to ≤ MAX_METADATA_VALUE_LENGTH bytes and retry.", "—", False),
    "OraclePriceInvalid":             ("Treat as terminal for this request; investigate oracle data feed.", "OracleConfigUpdatedEvent", False),
    "InvalidExpiration":              ("Fix expires_at to a future ledger timestamp and retry.", "—", False),
    # State transition
    "InvalidStatusTransition":        ("Refresh subscription state before presenting the next action.", "—", False),
    "NotActive":                      ("Refresh state; do not blindly retry.", "—", False),
    "SubscriptionExpired":            ("Stop retrying mutating operations on this subscription.", "SubscriptionExpiredEvent", False),
    "IntervalNotElapsed":             ("Retry only after next_charge_timestamp reported by get_next_charge_info.", "—", False),
    "Replay":                         ("Treat as idempotent duplicate; do not retry with a new key for the same action.", "—", False),
    "RecoveryNotAllowed":             ("Stop and inspect subscription state or policy before retrying.", "RecoveryEvent", False),
    "EmergencyStopActive":            ("Pause writes; poll get_emergency_stop_status and retry after admin clears stop.", "EmergencyStopDisabledEvent", False),
    "AlreadyInitialized":             ("Do not retry; contract is already set up.", "—", False),
    "MerchantPaused":                 ("Retry only after merchant pause is removed (unpause_merchant).", "MerchantUnpausedEvent", False),
    "Reentrancy":                     ("Treat as a security failure; investigate calling path immediately.", "—", False),
    "NotInGracePeriod":               ("Refresh state; a grace-period buyout is only legal when status == GracePeriod.", "—", False),
    "TimelockNotElapsed":             ("Wait until the scheduled effective timestamp; this is a timelock, not an error condition.", "TreasuryChangeScheduledEvent", False),
    "VacationActive":                 ("Retry after the merchant vacation window ends or the merchant calls unpause_merchant.", "MerchantUnpausedEvent", False),
    "UsageLimitsRequired":            ("Call configure_usage_limits for this subscription before charging usage.", "UsageLimitsConfiguredEvent", False),
    "EmergencyWithdrawInvalidState":  ("Refresh subscription state; emergency withdraw is only valid from the states the policy allows.", "—", False),
    # Accounting
    "InsufficientBalance":            ("Retry only after subscriber deposits funds via deposit_funds.", "FundsDepositedEvent", False),
    "InsufficientPrepaidBalance":     ("Top up subscription via deposit_funds, then retry.", "FundsDepositedEvent", False),
    "BelowMinimumTopup":              ("Increase deposit amount above get_min_topup() threshold and retry.", "—", False),
    "Underflow":                      ("Treat as terminal; investigate accounting invariant violation; not user-retriable.", "—", False),
    "Overflow":                       ("Treat as terminal; investigate arithmetic overflow; not user-retriable.", "—", False),
    "OracleNotConfigured":            ("Admin must call set_oracle_config with a valid oracle address.", "OracleConfigUpdatedEvent", False),
    "OraclePriceUnavailable":         ("Retry only after oracle data feed recovers.", "OracleChargeResolvedEvent", False),
    "OraclePriceStale":               ("Retry only after a fresh oracle quote is published.", "OracleChargeResolvedEvent", False),
    "OracleDeviationTooHigh":         ("Treat as terminal; the oracle price deviated past the circuit-breaker threshold. Investigate the price feed before retrying.", "OracleChargeResolvedEvent", False),
    "ProtocolFeeTooHigh":            ("Fix protocol_fee_bps to be at most MAX_PROTOCOL_FEE_BIPS.", "ProtocolFeeUpdatedEvent", False),
    "SelfReferralNotAllowed":        ("Use a different inviter address; an account cannot refer itself.", "—", False),
    # Limits
    "SubscriptionLimitReached":       ("Treat as terminal capacity failure; no new subscriptions can be created.", "—", False),
    "LifetimeCapReached":             ("Stop charging; surface terminal state to user.", "LifetimeCapReachedEvent", False),
    "UsageNotEnabled":                ("Fix request — subscription was created with usage_enabled=false.", "—", False),
    "InvalidExportLimit":             ("Fix pagination limit to [1, 100].", "—", False),
    "MetadataKeyLimitReached":        ("Delete or update existing keys (up to MAX_METADATA_KEYS) before retrying.", "MetadataDeletedEvent", False),
    "MaxConcurrentSubscriptionsReached": ("Subscriber already at plan concurrency limit; cancel an existing subscription first.", "SubscriptionCancelledEvent", False),
    "CreditLimitExceeded":            ("Reduce deposit / subscription amount or raise limit via set_subscriber_credit_limit.", "—", False),
    "RateLimitExceeded":              ("Retry after the rate window resets (see configure_usage_limits).", "UsageLimitsConfiguredEvent", False),
    "UsageCapExceeded":               ("Retry only after new billing period begins or cap is raised.", "UsageLimitsConfiguredEvent", False),
    "BurstLimitExceeded":             ("Retry after burst_min_interval_secs elapses.", "UsageLimitsConfiguredEvent", False),
    "SubscriberRateLimited":         ("Retry after the subscriber creation rate window resets; check get_subscriber_create_window.", "—", False),
    "IdemBufferFull":                 ("All 64 idempotency slots are occupied by live (non-expired) entries. Wait until the oldest entry's TTL elapses (up to 7 days from its insertion timestamp) before retrying. Under normal billing cadences this error cannot occur; if seen, investigate abnormally high charge frequency on this subscription.", "—", False),
    # Merchant config
    "InvalidFeeBips":                 ("Fix fee_bips to be in range [0, 10000].", "MerchantConfigUpdatedEvent", False),
    "InvalidOperations":              ("Fix allowed_operations bitmap to use only valid OP_* bits.", "MerchantConfigUpdatedEvent", False),
    "MustAllowChargeOperation":       ("Set OP_CHARGE bit in allowed_operations; merchants must accept charges.", "MerchantConfigUpdatedEvent", False),
    "MerchantNotApproved":            ("Request admin approval or disable whitelist mode via set_merchant_whitelist_mode.", "—", False),
    "UnknownMerchantTag":             ("Fix input; call get_tag_allowlist and use only listed tags.", "—", False),
    "DuplicateMerchantTag":           ("Remove the repeated tag from the request and retry.", "—", False),
    "MerchantTagLimitExceeded":       ("Reduce tags to at most MAX_MERCHANT_TAGS before retrying.", "—", False),
    # Token
    "InvalidTokenDecimals":           ("Fix token_decimals; must be in [1, 19].", "—", False),
    "InvalidToken":                   ("Provide an accepted token address from list_accepted_tokens.", "—", False),
    # Subscription update
    "CannotChangeUsageMode":          ("Cannot toggle usage_enabled on an existing subscription; create a new one.", "—", False),
    # Schema migration
    "SchemaMigrationDowngrade":       ("Downgrade rejected; deploy the correct binary version.", "SchemaMigratedEvent", False),
    "SchemaVersionMismatch":          ("Migration rejected; deploy compatible binary.", "—", False),
    "SchemaMigrationVerificationFailed": ("Treat as terminal; investigate migrated storage before retrying.", "—", False),
    # Dispute / Chargeback
    "DisputeNotFound":                ("Verify dispute ID before retrying.", "—", False),
    "DisputeAlreadyResolved":         ("Inspect existing resolution; do not retry.", "DisputeResolvedEvent", False),
    "DisputeNotResponded":            ("Wait for admin response or dispute window to elapse.", "DisputeRespondedEvent", False),
    "DisputeWindowElapsed":           ("Check auto-resolution rules; dispute can now be resolved.", "—", False),
    "DisputeAlreadyOpen":             ("A dispute is already open for this subscription; wait for resolution.", "DisputeOpenedEvent", False),
    "DisputeAlreadyResponded":        ("Dispute is not in `Open` status; cannot respond twice.", "DisputeRespondedEvent", False),
    "DisputeOverpay":                  ("Fix the resolution split so total disbursed does not exceed the escrowed amount.", "DisputeResolvedEvent", False),
    "SubscriberHasOpenDisputes":      ("Resolve the subscriber's open disputes before requesting blocklist removal.", "DisputeResolvedEvent", False),
    "DisputeAmountExceedsMerchantBalance": ("Fix the dispute resolution amount; merchant balance is insufficient to cover the payout.", "DisputeResolvedEvent", False),
    # Subscription transfer (11000-11099)
    "TransferIntentNotFound":         ("Verify transfer initiation or expiry before retrying.", "—", False),
    "TransferIntentExpired":          ("Transfer intent has expired; initiate a new transfer.", "—", False),
    "InvalidTransferTarget":          ("Provide a valid target address (not self).", "—", False),
    # Cooldown / lifecycle window (12000-12099)
    "CooldownActive":                 ("Wait for the per-key cooldown to elapse before mutating this config value.", "—", False),
    "RenewalWindowClosed":            ("The auto-renewal window has closed; cancel and recreate the subscription to resume billing.", "—", False),
    "EmergencyWithdrawCooldownActive":("Wait for the emergency-withdraw cooldown to elapse before requesting again.", "—", False),
    "EmergencyWithdrawNotRequested":  ("No open emergency-withdraw request exists, or it was already finalized. Request one first.", "—", False),
    "EmergencyWithdrawStateChanged":  ("Subscription state changed since the request; read current state and request a new emergency withdraw.", "—", False),
    # Delegated payer & escrow (13000-13099)
    "DelegatedPayerGrantNotFound":    ("Create a delegated-payer grant for this subscriber before depositing on their behalf.", "—", False),
    "DelegatedPayerGrantExpired":     ("Create a fresh delegated-payer grant; the existing one has expired.", "—", False),
    "DelegatedPayerAmountExceeded":   ("Reduce the deposit to at most the grant's max_amount, or raise the grant limit.", "—", False),
    "EscrowNotFound":                 ("No cancellation escrow exists for this subscription; verify the subscription id.", "—", False),
    "EscrowNotReleased":              ("Wait for the cancellation escrow release window to elapse before withdrawing.", "—", False),
    # Admin rotation (14000-14099)
    "ProposalNotFound":               ("No admin proposal is active. Call propose_admin first.", "—", False),
    "ProposalExpired":                ("The 7-day claim window elapsed; call propose_admin again to start a fresh proposal.", "—", False),
    "InvalidClaimant":                ("Only the proposed new-admin address may call claim_admin_role.", "—", False),
    "ProposalAlreadyExists":          ("Cancel the active proposal with cancel_admin_proposal before proposing a different admin.", "AdminProposalCancelledEvent", False),
    "NoActiveProposal":               ("There is no active proposal to cancel; nothing to roll back.", "—", False),
    "ProposalCooldownActive":         ("Wait for the admin proposal cooldown to elapse before proposing again.", "—", False),
        "ProposalInvalidated":            ("Submit a new proposal after the admin rotation; pre-rotation proposals cannot be reused.", "—", False),
    # Multi-sig (15000-15099)
    "MultiSigApprovalRequired":       ("This operation needs multi-sig approval; submit a proposal and gather the required signatures first.", "MultiSigProposalCreatedEvent", False),
    "MultiSigProposalNotFound":       ("No valid multi-sig proposal matches this operation; create one first.", "MultiSigProposalCreatedEvent", False),
    "MultiSigQuorumNotReached":       ("Collect more signatures until the threshold is met, then execute.", "MultiSigProposalApprovedEvent", False),
    "MultiSigProposalExpired":        ("The multi-sig proposal expired; create a new proposal and re-collect signatures.", "—", False),
    # Coupon
    "CouponNotFound":                 ("Verify the coupon code before retrying.", "—", False),
    "CouponExpired":                  ("Coupon has expired; request a new coupon from the merchant.", "CouponCreatedEvent", False),
    "CouponRedemptionLimitReached":   ("Coupon has reached its global redemption limit; merchant may create a new coupon.", "CouponCreatedEvent", False),
    "CouponRevoked":                  ("Coupon was revoked by the merchant; choose a different coupon.", "CouponRevokedEvent", False),
    "CouponAlreadyExists":            ("Choose a different coupon code and retry.", "CouponCreatedEvent", False),
    "CouponAlreadyApplied":           ("Subscription already has a coupon bound; only one coupon per subscription.", "CouponAppliedEvent", False),
    "CouponTokenMismatch":            ("Use a coupon whose token matches the subscription's settlement token.", "—", False),
    # Subscription Transfer
    "TransferIntentNotFound":         ("Verify transfer initiation or expiry before retrying.", "—", False),
    "TransferIntentExpired":          ("Transfer intent has expired; initiate a new transfer.", "—", False),
    "InvalidTransferTarget":          ("Provide a valid target address (not self).", "—", False),
}
# fmt: on

# ---------------------------------------------------------------------------
# Data model
# ---------------------------------------------------------------------------


class Variant(NamedTuple):
    name: str
    code: int
    category: str
    doc: str
    deprecated: bool


# ---------------------------------------------------------------------------
# Step 1 – Parse types.rs
# ---------------------------------------------------------------------------

# Matches variants like:
#   /// Some doc comment
#   SomeVariant = 1234,
_VARIANT_RE = re.compile(
    r"///\s*(?P<doc>[^\n]+)\n\s+(?P<name>[A-Z][A-Za-z0-9]+)\s*=\s*(?P<code>\d+)",
    re.MULTILINE,
)

# Category ranges
_CATEGORY_RANGES = [
    (1000, 1099, "Auth"),
    (2000, 2099, "Not Found"),
    (3000, 3099, "Invalid Args"),
    (4000, 4099, "State Transition"),
    (5000, 5099, "Accounting"),
    (6000, 6099, "Limits"),
    (7000, 7099, "Merchant Config"),
    (8000, 8099, "Token"),
    (9000, 9099, "Subscription Update"),
    (9100, 9199, "Schema Migration"),
    (10000, 10099, "Dispute"),
    (11000, 11099, "Subscription Transfer"),
    (12000, 12099, "Cooldown"),
    (13000, 13099, "Delegated Payer"),
    (14000, 14099, "Admin Rotation"),
    (15000, 15099, "Multi-Sig"),
]


def _category(code: int) -> str:
    for lo, hi, label in _CATEGORY_RANGES:
        if lo <= code <= hi:
            return label
    return "Unknown"


def parse_variants(types_rs: Path) -> list[Variant]:
    """Extract all Error variants from types.rs."""
    text = types_rs.read_text(encoding="utf-8")

    # Isolate the Error enum body
    enum_match = re.search(
        r"pub enum Error \{(.+?)\n\}", text, re.DOTALL
    )
    if not enum_match:
        raise ValueError(f"Could not locate `pub enum Error` in {types_rs}")

    body = enum_match.group(1)

    variants: list[Variant] = []
    for m in _VARIANT_RE.finditer(body):
        name = m.group("name")
        code = int(m.group("code"))
        doc = m.group("doc").strip()
        cat = _category(code)
        _, _, deprecated = REMEDIATION.get(name, ("", "", False))
        variants.append(Variant(name=name, code=code, category=cat, doc=doc, deprecated=deprecated))

    return sorted(variants, key=lambda v: v.code)


# ---------------------------------------------------------------------------
# Step 2 – Grep source files for entrypoint occurrences
# ---------------------------------------------------------------------------

# Map from (module file stem) → friendly display label
_FILE_LABEL: dict[str, str] = {
    "lib":          "lib.rs",
    "admin":        "admin.rs",
    "charge_core":  "charge_core.rs",
    "subscription": "subscription.rs",
    "merchant":     "merchant.rs",
    "queries":      "queries.rs",
    "blocklist":    "blocklist.rs",
    "metadata":     "metadata.rs",
    "state_machine":"state_machine.rs",
    "nonce":        "nonce.rs",
    "reentrancy":   "reentrancy.rs",
    "operator":     "lib.rs (operator)",
    "dispute":      "dispute.rs",
}

# Entrypoints that are exposed to callers — derived from lib.rs impl block.
# Used to narrow the display from raw file-name to the public entrypoint.
_ENTRYPOINTS_IN_LIB: list[str] = [
    "init", "set_min_topup", "get_min_topup", "get_admin", "get_admin_nonce",
    "set_operator", "remove_operator", "get_operator", "get_operator_nonce",
    "operator_batch_charge", "operator_charge_subscription", "operator_charge_usage",
    "operator_charge_usage_with_ref", "rotate_admin", "set_oracle_config",
    "recover_stranded_funds", "batch_charge", "get_emergency_stop_status",
    "enable_emergency_stop", "disable_emergency_stop", "migrate",
    "export_contract_snapshot", "export_subscription_summary",
    "export_subscription_summaries", "create_subscription",
    "create_subscription_with_token", "create_subscription_from_plan",
    "deposit_funds", "create_plan_template", "create_plan_template_with_token",
    "get_plan_template", "update_plan_template", "set_plan_max_active_subs",
    "get_plan_max_active_subs", "migrate_subscription_to_plan",
    "set_subscriber_credit_limit", "get_subscriber_credit_limit",
    "get_subscriber_exposure", "cancel_subscription", "withdraw_subscriber_funds",
    "partial_refund", "pause_subscription", "resume_subscription",
    "cleanup_subscription", "charge_one_off", "charge_subscription",
    "charge_usage", "charge_usage_with_reference", "configure_usage_limits",
    "withdraw_merchant_funds", "withdraw_merchant_token_funds",
    "get_merchant_balance", "get_merchant_balance_by_token",
    "get_merchant_token_earnings", "get_merchant_paused", "pause_merchant",
    "unpause_merchant", "merchant_refund", "get_reconciliation_snapshot",
    "get_merchant_total_earnings", "get_subscription",
    "estimate_topup_for_intervals", "get_next_charge_info",
    "get_token_subscription_count", "list_subscriptions_by_subscriber",
    "get_cap_info", "set_global_cap_default", "get_global_cap_default",
    "set_merchant_cap_default", "get_merchant_cap_default",
    "update_subscription_cap", "get_sub_statements_offset",
    "get_sub_statements_cursor", "get_period_snapshot", "list_period_snapshots",
    "add_accepted_token", "remove_accepted_token", "list_accepted_tokens",
    "get_subscriptions_by_token", "get_token_reconciliation", "get_recon_summary",
    "generate_reconciliation_proof", "query_prepaid_balances_paginated",
    "set_billing_retention", "get_billing_retention", "get_stmt_compacted_aggregate",
    "compact_billing_statements", "get_oracle_config", "set_metadata",
    "delete_metadata", "get_metadata", "list_metadata_keys", "set_protocol_fee",
    "get_protocol_fee_bps", "add_to_blocklist", "remove_from_blocklist",
    "get_blocklist_entry", "is_blocklisted", "initialize_merchant_config",
    "set_merchant_config", "update_merchant_config", "get_merchant_config",
    "version", "get_subscription_count",
    "open_dispute", "respond_dispute", "resolve_dispute",
    "get_dispute", "get_subscription_dispute",
]


def grep_entrypoints(src_dir: Path, variant_name: str) -> list[str]:
    """Return a sorted, deduplicated list of files that reference Error::<variant_name>."""
    pattern = re.compile(r"\bError::" + re.escape(variant_name) + r"\b")
    hits: set[str] = set()

    for rs_file in src_dir.glob("*.rs"):
        # Security: ensure the file is actually inside src_dir
        try:
            rs_file.resolve().relative_to(src_dir.resolve())
        except ValueError:  # pragma: no cover
            continue

        try:
            text = rs_file.read_text(encoding="utf-8")
        except OSError:  # pragma: no cover
            continue

        if pattern.search(text):
            stem = rs_file.stem
            label = _FILE_LABEL.get(stem, rs_file.name)
            hits.add(label)

    return sorted(hits)


# ---------------------------------------------------------------------------
# Step 3 – Build the Markdown table
# ---------------------------------------------------------------------------

_TABLE_HEADER = """\
| Code | Variant | Category | Emitting entrypoints (modules) | Recovery action | Related event |
|---:|:---|:---|:---|:---|:---|"""

_DEPRECATED_NOTE = " ~~(deprecated)~~"


def build_table(variants: list[Variant], src_dir: Path) -> str:
    lines = [_TABLE_HEADER]
    undocumented: list[str] = []

    for v in variants:
        entrypoints = grep_entrypoints(src_dir, v.name)
        ep_cell = ", ".join(f"`{e}`" for e in entrypoints) if entrypoints else "—"

        if v.name in REMEDIATION:
            recovery, event, _ = REMEDIATION[v.name]
        else:
            recovery = "⚠ No remediation documented — add entry to `REMEDIATION` map in script."
            event = "—"
            undocumented.append(v.name)

        name_cell = f"`{v.name}`"
        if v.deprecated:
            name_cell += _DEPRECATED_NOTE

        lines.append(
            f"| {v.code} | {name_cell} | {v.category} | {ep_cell} | {recovery} | {event} |"
        )

    table = "\n".join(lines)
    return table, undocumented


# ---------------------------------------------------------------------------
# Step 4 – Splice into errors.md
# ---------------------------------------------------------------------------

_SECTION_HEADER = """\
## Entrypoint Cross-Reference

This table is **generated** by `scripts/generate_error_table.py` and kept in sync
by CI (see `.github/workflows/docs.yml`). Do not edit the block between the
sentinel comments manually — run the script instead.

Column definitions:
- **Emitting entrypoints**: source modules that contain `Error::<Variant>`.
  The public entrypoint name as exposed in `lib.rs` is listed where it differs
  from the internal module name.
- **Recovery action**: recommended remediation for integrators.
- **Related event**: Soroban event type emitted alongside this error, where applicable.

"""


def splice_table(errors_md: Path, table_text: str, undocumented: list[str]) -> str:
    """Return the new file content with the generated block replaced."""
    original = errors_md.read_text(encoding="utf-8")

    # Build replacement block
    undoc_section = ""
    if undocumented:
        names = ", ".join(f"`{n}`" for n in undocumented)
        undoc_section = (
            "\n\n> **⚠ Undocumented variants** — the following variants are present in "
            f"`types.rs` but have no entry in the script's `REMEDIATION` map: {names}. "
            "Add them to `scripts/generate_error_table.py` to resolve this warning.\n"
        )

    replacement = (
        START_SENTINEL
        + "\n"
        + _SECTION_HEADER
        + table_text
        + undoc_section
        + "\n"
        + END_SENTINEL
    )

    if START_SENTINEL in original and END_SENTINEL in original:
        # Replace existing block
        new_content = re.sub(
            re.escape(START_SENTINEL) + r".*?" + re.escape(END_SENTINEL),
            replacement,
            original,
            flags=re.DOTALL,
        )
    else:
        # Append at end
        new_content = original.rstrip("\n") + "\n\n" + replacement + "\n"

    return new_content


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="Exit with status 1 when docs/errors.md would change (CI staleness check).",
    )
    parser.add_argument(
        "--repo-root",
        type=Path,
        default=REPO_ROOT,
        help="Override repository root (default: parent of scripts/).",
    )
    args = parser.parse_args(argv)

    repo_root: Path = args.repo_root.resolve()
    types_rs = repo_root / "contracts" / "subscription_vault" / "src" / "types.rs"
    src_dir = repo_root / "contracts" / "subscription_vault" / "src"
    errors_md = repo_root / "docs" / "errors.md"

    # Validate paths exist
    for p in (types_rs, src_dir, errors_md):
        if not p.exists():
            print(f"ERROR: expected path does not exist: {p}", file=sys.stderr)
            return 2

    variants = parse_variants(types_rs)
    if not variants:
        print("ERROR: No Error variants found — check types.rs parsing.", file=sys.stderr)  # pragma: no cover
        return 2  # pragma: no cover

    table_text, undocumented = build_table(variants, src_dir)
    new_content = splice_table(errors_md, table_text, undocumented)

    current_content = errors_md.read_text(encoding="utf-8")
    is_stale = new_content != current_content

    # ── Undocumented-variant gate (issue #218) ───────────────────────────────
    # A variant present in `types.rs` but absent from the `REMEDIATION` map has
    # no documented handling, so integrators receive it with an
    # "⚠ No remediation documented" cell. That used to be advisory only: the
    # warning was written into the table but the process still exited 0, so a
    # new error could be merged with no documentation at all and CI stayed
    # green. Report it and fail instead.
    #
    # In write mode the file is still updated first, so a developer running the
    # generator locally gets the refreshed table *and* a non-zero exit telling
    # them the change is incomplete.
    undocumented_error = bool(undocumented)

    def _report_undocumented() -> None:
        print(
            f"FAIL: {len(undocumented)} error variant(s) in types.rs are missing from the "
            "REMEDIATION map in scripts/generate_error_table.py:",
            file=sys.stderr,
        )
        for name in undocumented:
            code = next((v.code for v in variants if v.name == name), None)
            print(f"  - {name} (code {code})", file=sys.stderr)
        print(
            "\nAdd each variant to the REMEDIATION map with its recovery action and "
            "related event, then re-run the generator and commit docs/errors.md.",
            file=sys.stderr,
        )

    if args.check:
        if undocumented_error:
            _report_undocumented()
            # Continue to the staleness check so both problems are reported at once.
        if is_stale:
            print(
                "FAIL: docs/errors.md is stale. Run `python scripts/generate_error_table.py` "
                "and commit the result.",
                file=sys.stderr,
            )
            return 1
        if undocumented_error:
            return 1
        print("OK: docs/errors.md is up to date.")
        return 0

    if is_stale:
        errors_md.write_text(new_content, encoding="utf-8")
        print(f"Updated {errors_md.relative_to(repo_root)}")
        print(f"  {len(variants)} variants processed, {len(undocumented)} undocumented.")
    else:
        print(f"No changes needed in {errors_md.relative_to(repo_root)}.")

    if undocumented_error:
        _report_undocumented()
        return 1

    return 0


if __name__ == "__main__":  # pragma: no cover
    sys.exit(main())
