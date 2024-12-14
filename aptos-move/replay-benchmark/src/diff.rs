// Copyright (c) Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use aptos_types::{
    contract_event::ContractEvent,
    fee_statement::FeeStatement,
    state_store::state_key::StateKey,
    transaction::{ExecutionStatus, TransactionOutput},
    write_set::{WriteOp, WriteSet, TOTAL_SUPPLY_STATE_KEY},
};
use claims::assert_ok;
use move_core_types::{language_storage::TypeTag, move_resource::MoveStructType};
use std::collections::BTreeMap;

/// Different parts of [TransactionOutput] that can be different:
///   1. gas used,
///   2. status (must be kept since transactions are replayed),
///   3. events,
///   4. writes.
/// Note that fine-grained comparison allows for some differences to be okay, e.g., using more gas
/// implies that the fee statement event, the account balance of the fee payer, and the total token
/// supply are different.
#[derive(Eq, PartialEq)]
enum Diff {
    GasUsed {
        left: u64,
        right: u64,
    },
    ExecutionStatus {
        left: ExecutionStatus,
        right: ExecutionStatus,
    },
    Event {
        left: Option<ContractEvent>,
        right: Option<ContractEvent>,
    },
    WriteSet {
        state_key: StateKey,
        left: Option<WriteOp>,
        right: Option<WriteOp>,
    },
}

/// Holds all differences for a pair of transaction outputs.
pub(crate) struct TransactionDiff {
    diffs: Vec<Diff>,
}

/// Builds [TransactionDiff]s for transaction outputs. The builder can be configured to ignore the
/// differences in outputs sometimes.
pub(crate) struct TransactionDiffBuilder {
    /// If true, differences related to the gas usage are ignored. These include:
    ///   - total gas used is not compared,
    ///   - `EmitFeeStatement` event is not compared,
    ///   - total APT supply is not compared,
    ///   - account balances are no compared.
    /// Note that for
    allow_different_gas_usage: bool,
}

impl TransactionDiffBuilder {
    #[allow(clippy::new_without_default)]
    pub(crate) fn new() -> Self {
        Self {
            allow_different_gas_usage: false,
        }
    }

    /// Given a pair of transaction outputs, computes its [TransactionDiff] that includes the gas
    /// used, execution status, events and write sets.
    pub(crate) fn build_from_outputs(
        &self,
        left: TransactionOutput,
        right: TransactionOutput,
    ) -> TransactionDiff {
        let (left_write_set, left_events, left_gas_used, left_transaction_status, _) =
            left.unpack();
        let (right_write_set, right_events, right_gas_used, right_transaction_status, _) =
            right.unpack();

        let mut diffs = vec![];

        // All statuses must be kept, since we are replaying transactions.
        let left_execution_status = assert_ok!(left_transaction_status.as_kept_status());
        let right_execution_status = assert_ok!(right_transaction_status.as_kept_status());
        if left_execution_status != right_execution_status {
            diffs.push(Diff::ExecutionStatus {
                left: left_execution_status,
                right: right_execution_status,
            });
        }

        if left_gas_used != right_gas_used && !self.allow_different_gas_usage {
            diffs.push(Diff::GasUsed {
                left: left_gas_used,
                right: right_gas_used,
            });
        }

        diffs.extend(self.diff_events(left_events, right_events));
        diffs.extend(self.diff_write_sets(left_write_set, right_write_set));

        TransactionDiff { diffs }
    }

    /// Computes the differences between a pair of event vectors.
    fn diff_events(&self, left: Vec<ContractEvent>, right: Vec<ContractEvent>) -> Vec<Diff> {
        let event_vec_to_map = |events: Vec<ContractEvent>| {
            events
                .into_iter()
                .map(|event| (event.type_tag().clone(), event))
                .collect::<BTreeMap<_, _>>()
        };

        let left = event_vec_to_map(left);
        let mut right = event_vec_to_map(right);

        let mut diffs = vec![];
        for (left_ty_tag, left_event) in left {
            let maybe_right_event = right.remove(&left_ty_tag);
            if maybe_right_event
                .as_ref()
                .is_some_and(|right_event| left_event.event_data() == right_event.event_data())
            {
                continue;
            }

            // If there are two fee statement events, and we allow different gas usage - ignore the
            // comparison.
            if self.allow_different_gas_usage
                && left_ty_tag == TypeTag::Struct(Box::new(FeeStatement::struct_tag()))
                && maybe_right_event.is_some()
            {
                continue;
            }

            diffs.push(Diff::Event {
                left: Some(left_event),
                right: maybe_right_event,
            });
        }

        for right_event in right.into_values() {
            diffs.push(Diff::Event {
                left: None,
                right: Some(right_event),
            });
        }
        diffs
    }

    /// Computes the differences between a pair of write sets.
    fn diff_write_sets(&self, left: WriteSet, right: WriteSet) -> Vec<Diff> {
        let left = left.into_mut().into_inner();
        let mut right = right.into_mut().into_inner();

        let mut diffs = vec![];
        for (left_state_key, left_write_op) in left {
            let maybe_right_write_op = right.remove(&left_state_key);
            if maybe_right_write_op
                .as_ref()
                .is_some_and(|right_write_op| right_write_op == &left_write_op)
            {
                continue;
            }

            // Skip total APT supply comparisons. Those should always be part of the write set.
            if self.allow_different_gas_usage && left_state_key == *TOTAL_SUPPLY_STATE_KEY {
                continue;
            }
            // TODO: compare accounts. How do we ensure that account related change is a fee change.

            diffs.push(Diff::WriteSet {
                state_key: left_state_key,
                left: Some(left_write_op),
                right: maybe_right_write_op,
            });
        }

        for (right_state_key, right_write_op) in right {
            diffs.push(Diff::WriteSet {
                state_key: right_state_key,
                left: None,
                right: Some(right_write_op),
            });
        }
        diffs
    }
}

impl std::fmt::Display for TransactionDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, " >>>>> ")?;
        for diff in &self.diffs {
            match diff {
                Diff::GasUsed { left, right } => {
                    writeln!(f, "[gas used] before: {}, after: {}", left, right)?;
                },
                Diff::ExecutionStatus { left, right } => {
                    writeln!(
                        f,
                        "[execution status] before: {:?}, after: {:?}",
                        left, right
                    )?;
                },
                Diff::Event { left, right } => {
                    let left = left.as_ref();
                    let right = right.as_ref();

                    if left.is_none() {
                        writeln!(
                            f,
                            "[event] {} was not emitted before",
                            right.unwrap().type_tag().to_canonical_string()
                        )?;
                    } else if right.is_none() {
                        writeln!(
                            f,
                            "[event] {} is not emitted anymore",
                            left.unwrap().type_tag().to_canonical_string()
                        )?;
                    } else {
                        writeln!(
                            f,
                            "[event] {} has changed its data",
                            left.unwrap().type_tag().to_canonical_string()
                        )?;
                    }
                },
                Diff::WriteSet {
                    state_key,
                    left,
                    right,
                } => {
                    let left = left.as_ref();
                    let right = right.as_ref();

                    if left.is_none() {
                        writeln!(f, "[write] {:?} was not written to before", state_key)?;
                    } else if right.is_none() {
                        writeln!(f, "[write] {:?} is not written to anymore", state_key)?;
                    } else {
                        writeln!(f, "[write] {:?} has changed its value", state_key)?;
                    }
                },
            }
        }
        writeln!(f, " <<<<< ")
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use aptos_types::{
        on_chain_config::CurrentTimeMicroseconds, state_store::state_value::StateValueMetadata,
        write_set::WriteSetMut,
    };

    #[test]
    fn test_diff_events() {
        let events_1 = vec![
            ContractEvent::new_v2_with_type_tag_str("0x1::event::EventA", vec![0, 1, 2]),
            ContractEvent::new_v2_with_type_tag_str("0x1::event::EventB", vec![0, 1, 2]),
            ContractEvent::new_v2_with_type_tag_str("0x1::event::EventD", vec![0, 1, 2]),
        ];

        let events_2 = vec![
            ContractEvent::new_v2_with_type_tag_str("0x1::event::EventA", vec![0, 1, 2]),
            ContractEvent::new_v2_with_type_tag_str("0x1::event::EventC", vec![0, 1, 2]),
            ContractEvent::new_v2_with_type_tag_str("0x1::event::EventD", vec![0, 1, 3]),
        ];

        let expected_diffs = vec![
            Diff::Event {
                left: Some(ContractEvent::new_v2_with_type_tag_str(
                    "0x1::event::EventB",
                    vec![0, 1, 2],
                )),
                right: None,
            },
            Diff::Event {
                left: None,
                right: Some(ContractEvent::new_v2_with_type_tag_str(
                    "0x1::event::EventC",
                    vec![0, 1, 2],
                )),
            },
            Diff::Event {
                left: Some(ContractEvent::new_v2_with_type_tag_str(
                    "0x1::event::EventD",
                    vec![0, 1, 2],
                )),
                right: Some(ContractEvent::new_v2_with_type_tag_str(
                    "0x1::event::EventD",
                    vec![0, 1, 3],
                )),
            },
        ];

        let diffs = TransactionDiffBuilder::new().diff_events(events_1, events_2);
        assert_eq!(diffs.len(), 3);
        assert!(diffs.iter().all(|diff| expected_diffs.contains(diff)));
    }

    #[test]
    fn test_diff_write_sets() {
        let write_set_1 = WriteSetMut::new(vec![
            // Same in 2nd write set.
            (
                StateKey::raw(b"key-1"),
                WriteOp::legacy_creation(vec![0, 1, 2].into()),
            ),
            // Does not exist in 2nd write set.
            (
                StateKey::raw(b"key-2"),
                WriteOp::legacy_creation(vec![0, 1, 2].into()),
            ),
            // Different from 2nd write-set.
            (
                StateKey::raw(b"key-4"),
                WriteOp::legacy_creation(vec![0, 1, 2].into()),
            ),
            (
                StateKey::raw(b"key-5"),
                WriteOp::legacy_creation(vec![0, 1, 2].into()),
            ),
            (
                StateKey::raw(b"key-6"),
                WriteOp::creation(
                    vec![0, 1, 2].into(),
                    StateValueMetadata::new(1, 2, &CurrentTimeMicroseconds { microseconds: 100 }),
                ),
            ),
        ])
        .freeze()
        .unwrap();

        let write_set_2 = WriteSetMut::new(vec![
            // Same in 1st write set.
            (
                StateKey::raw(b"key-1"),
                WriteOp::legacy_creation(vec![0, 1, 2].into()),
            ),
            // Does nto exist in 1st write set.
            (
                StateKey::raw(b"key-3"),
                WriteOp::legacy_creation(vec![0, 1, 2].into()),
            ),
            // Different from 1st write-set.
            (
                StateKey::raw(b"key-4"),
                WriteOp::legacy_creation(vec![0, 1, 3].into()),
            ),
            (
                StateKey::raw(b"key-5"),
                WriteOp::legacy_modification(vec![0, 1, 2].into()),
            ),
            (
                StateKey::raw(b"key-6"),
                WriteOp::creation(
                    vec![0, 1, 2].into(),
                    StateValueMetadata::new(1, 2, &CurrentTimeMicroseconds { microseconds: 200 }),
                ),
            ),
        ])
        .freeze()
        .unwrap();

        let expected_diffs = vec![
            Diff::WriteSet {
                state_key: StateKey::raw(b"key-2"),
                left: Some(WriteOp::legacy_creation(vec![0, 1, 2].into())),
                right: None,
            },
            Diff::WriteSet {
                state_key: StateKey::raw(b"key-3"),
                left: None,
                right: Some(WriteOp::legacy_creation(vec![0, 1, 2].into())),
            },
            Diff::WriteSet {
                state_key: StateKey::raw(b"key-4"),
                left: Some(WriteOp::legacy_creation(vec![0, 1, 2].into())),
                right: Some(WriteOp::legacy_creation(vec![0, 1, 3].into())),
            },
            Diff::WriteSet {
                state_key: StateKey::raw(b"key-5"),
                left: Some(WriteOp::legacy_creation(vec![0, 1, 2].into())),
                right: Some(WriteOp::legacy_modification(vec![0, 1, 2].into())),
            },
            Diff::WriteSet {
                state_key: StateKey::raw(b"key-6"),
                left: Some(WriteOp::creation(
                    vec![0, 1, 2].into(),
                    StateValueMetadata::new(1, 2, &CurrentTimeMicroseconds { microseconds: 100 }),
                )),
                right: Some(WriteOp::creation(
                    vec![0, 1, 2].into(),
                    StateValueMetadata::new(1, 2, &CurrentTimeMicroseconds { microseconds: 200 }),
                )),
            },
        ];

        let diffs = TransactionDiffBuilder::new().diff_write_sets(write_set_1, write_set_2);
        assert_eq!(diffs.len(), 5);
        assert!(diffs.iter().all(|diff| expected_diffs.contains(diff)));
    }
}
