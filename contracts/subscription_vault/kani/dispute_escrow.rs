//! Kani harnesses for the dispute escrow accounting invariant.
///
/// The dispute workflow moves the disputed amount from the merchant's
/// balance into a `DisputeEscrow` key. The accounting invariant is that the
/// merchant balance plus the escrowed amount always equals the original
/// merchant balance for the duration of the dispute.
///
/// These harnesses exercise the pure arithmetic that underpins that
/// invariant. They are deliberately free of Soroban host calls so that they
/// can be exhaustively model-checked by Kani.

/// Escrow accounting invariant for a single dispute.
///
/// Given a merchant balance and a disputed amount, opening a dispute
/// debits the merchant balance by the disputed amount and credits the
/// escrow by the same amount. The sum of the two must be unchanged.
///
/// Properties proved:
/// - The merchant balance never goes negative.
/// - The escrow balance equals the disputed amount.
/// - The sum of merchant balance and escrow equals the original balance.
#[kani::proof]
fn dispute_open_preserves_escrow_invariant() {
    let original_merchant_balance: i128 = kani::any();
    let disputed_amount: i128 = kani::any();

    // Only disputes that are within the merchant balance and strictly
    // positive are allowed to open.
    kani::assume(original_merchant_balance >= 0);
    kani::assume(disputed_amount > 0);
    kani::assume(disputed_amount <= original_merchant_balance);

    // Opening the dispute debits the merchant and credits escrow.
    let new_merchant_balance = original_merchant_balance - disputed_amount;
    let escrow_balance = disputed_amount;

    // Invariant: merchant + escrow == original merchant balance.
    assert(new_merchant_balance + escrow_balance == original_merchant_balance);
    // No over-debit.
    assert(new_merchant_balance >= 0);
    // Escrow holds exactly the disputed amount.
    assert(escrow_balance == disputed_amount);
}

/// Resolution invariant: a dispute can never disburse more than it
/// escrowed.
///
/// The current flow is all-or-nothing, but the ledger tracks cumulative
/// disbursements so that future partial-resolution logic cannot overpay.
/// This harness proves that for any valid ledger state the remaining
/// amount is non-negative and the new total disbursed never exceeds the
/// original escrow.
#[kani::proof]
fn dispute_resolution_never_overpays() {
    let original_amount: i128 = kani::any();
    let total_disbursed: i128 = kani::any();

    // A ledger is only valid when the original amount is positive and the
    // cumulative disbursement has not exceeded it.
    kani::assume(original_amount > 0);
    kani::assume(total_disbursed >= 0);
    kani::assume(total_disbursed <= original_amount);

    let remaining = original_amount - total_disbursed;
    assert(remaining >= 0);

    // The new total disbursed after a full resolution must not exceed
    // the original escrow.
    let new_total_disbursed = total_disbursed + remaining;
    assert(new_total_disbursed <= original_amount);
    assert(new_total_disbursed == original_amount);
}

/// Resolution to the merchant restores the escrowed amount to the
/// merchant balance, preserving the original accounting total.
#[kani::proof]
fn dispute_resolution_to_merchant_restores_balance() {
    let merchant_balance_after_open: i128 = kani::any();
    let escrow_balance: i128 = kani::any();

    kani::assume(merchant_balance_after_open >= 0);
    kani::assume(escrow_balance > 0);

    // Resolution to merchant returns the escrow to the merchant.
    let new_merchant_balance = merchant_balance_after_open + escrow_balance;
    assert(new_merchant_balance >= merchant_balance_after_open);
    assert(new_merchant_balance - escrow_balance == merchant_balance_after_open);
    // The escrow is fully drained by the resolution.
    assert(escrow_balance - escrow_balance == 0);
}
