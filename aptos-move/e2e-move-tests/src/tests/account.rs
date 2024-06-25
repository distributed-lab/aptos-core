// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{assert_abort, MoveHarness};
use aptos_cached_packages::aptos_stdlib::aptos_account_transfer;
use aptos_language_e2e_tests::account::Account;

#[test]
fn non_existent_sender() {
    let mut h = MoveHarness::new();

    let sender = Account::new();
    let receiver = h.new_account_with_balance_and_sequence_number(100_000, 0);

    let txn = sender
        .transaction()
        .payload(aptos_account_transfer(*receiver.address(), 10))
        .sequence_number(0)
        .sign();

    // lite_account will be created automatically.
    let status = h.run(txn);

    assert_abort!(status, 65542);
}
