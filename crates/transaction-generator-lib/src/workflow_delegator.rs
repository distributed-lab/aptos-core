// Copyright © Aptos Foundation
// SPDX-License-Identifier: Apache-2.0

use crate::{
    account_generator::AccountGeneratorCreator,
    accounts_pool_wrapper::{
        AccountsPoolWrapperCreator, AddHistoryWrapperCreator, MarketMakerPoolWrapperCreator,
        ReuseAccountsPoolWrapperCreator,
    },
    call_custom_modules::CustomModulesDelegationGeneratorCreator,
    econia_order_generator::{
        register_econia_markets, EconiaDepositCoinsTransactionGenerator,
        EconiaLimitOrderTransactionGenerator, EconiaMarketOrderTransactionGenerator,
        EconiaRealOrderTransactionGenerator, EconiaRegisterMarketUserTransactionGenerator,
    },
    entry_points::EntryPointTransactionGenerator,
    EconiaFlowType, EntryPoints, ObjectPool, ReliableTransactionSubmitter, RootAccountHandle,
    TransactionGenerator, TransactionGeneratorCreator, WorkflowKind, WorkflowProgress,
};
use aptos_logger::{info, sample, sample::SampleRate};
use aptos_sdk::{
    transaction_builder::TransactionFactory,
    types::{transaction::SignedTransaction, LocalAccount},
};
use std::{
    fmt::Debug,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug)]
enum StageTracking {
    // Stage is externally modified. This is used by executor benchmark tests
    ExternallySet(Arc<AtomicUsize>),
    // We move to a next stage when all accounts have finished with the current stage
    // This is used by transaction emitter (forge and tests on mainnet, devnet, testnet)
    WhenDone {
        stage_counter: Arc<AtomicUsize>,
        stage_start_time: Arc<AtomicU64>,
        delay_between_stages: Duration,
    },
}

impl StageTracking {
    fn current_timestamp() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    // fn load_current_stage(&self) -> Option<usize> {
    //     match self {
    //         StageTracking::ExternallySet(stage_counter) => {
    //             Some(stage_counter.load(Ordering::Relaxed))
    //         },
    //         StageTracking::WhenDone {
    //             stage_counter,
    //             stage_start_time,
    //             ..
    //         } => {
    //             if stage_start_time.load(Ordering::Relaxed) > Self::current_timestamp() {
    //                 None
    //             } else {
    //                 Some(stage_counter.load(Ordering::Relaxed))
    //             }
    //         },
    //     }
    // }
}

#[derive(Clone)]
pub enum Pool {
    AccountPool(Arc<ObjectPool<LocalAccount>>),
    AccountWithHistoryPool(Arc<ObjectPool<(LocalAccount, Vec<String>)>>),
}

impl Pool {
    fn len(&self) -> usize {
        match self {
            Pool::AccountPool(pool) => pool.len(),
            Pool::AccountWithHistoryPool(pool) => pool.len(),
        }
    }
}

/// Generator allowing for multi-stage workflows.
/// List of generators are passed:
/// gen_0, gen_1, ... gen_n
/// and on list of account pools, each representing accounts in between two stages:
/// pool_0, pool_1, ... pool_n-1
///
/// pool_i is filled by gen_i, and consumed by gen_i+1, and so there is one less pools than generators.
///
/// We start with stage 0, which calls gen_0 stage_switch_conditions[0].len() times, which populates pool_0 with accounts.
///
/// After that, in stage 1, we call gen_1, which consumes accounts from pool_0, and moves them to pool_1.
/// We do this until pool_0 is empty.
///
/// We proceed, until in the last stage - stage n - calls gen_n, which consumes accounts from pool_n-1.
///
/// There are two modes on when to move to the next stage:
/// - WhenDone means as soon as pool_i is empty, we move to stage i+1
/// - ExternallySet means we wait for external signal to move to next stage, and we stop creating transactions
///   until we receive it (or will move early if pool hasn't been consumed yet)
///
/// Use WorkflowTxnGeneratorCreator::create_workload to create this generator.
struct WorkflowTxnGenerator {
    stage: StageTracking,
    generators: Vec<Box<dyn TransactionGenerator>>,
    stage_switch_conditions: Vec<StageSwitchCondition>,
}

impl WorkflowTxnGenerator {
    fn new(
        stage: StageTracking,
        generators: Vec<Box<dyn TransactionGenerator>>,
        stage_switch_conditions: Vec<StageSwitchCondition>,
    ) -> Self {
        Self {
            stage,
            generators,
            stage_switch_conditions,
        }
    }
}

impl TransactionGenerator for WorkflowTxnGenerator {
    fn generate_transactions(
        &mut self,
        account: &LocalAccount,
        mut num_to_create: usize,
        _history: &[String],
        _market_maker: bool,
    ) -> Vec<SignedTransaction> {
        assert_ne!(num_to_create, 0);
        let stage = match &self.stage {
            StageTracking::ExternallySet(stage_counter) => stage_counter.load(Ordering::Relaxed),
            StageTracking::WhenDone {
                stage_counter,
                stage_start_time,
                ..
            } => {
                if stage_start_time.load(Ordering::Relaxed) > StageTracking::current_timestamp() {
                    info!("Waiting for next stage for {} seconds", 60);
                    thread::sleep(Duration::from_secs(60));
                }
                stage_counter.load(Ordering::Relaxed)
            },
        };

        match &self.stage {
            StageTracking::WhenDone {
                stage_counter,
                stage_start_time,
                delay_between_stages,
            } => {
                if stage < self.stage_switch_conditions.len()
                    && self
                        .stage_switch_conditions
                        .get(stage)
                        .unwrap()
                        .should_switch()
                {
                    info!("TransactionGenerator Workflow: Stage {} has consumed all accounts, moving to stage {}", stage, stage + 1);
                    stage_start_time.store(
                        StageTracking::current_timestamp() + delay_between_stages.as_secs(),
                        Ordering::Relaxed,
                    );
                    let _ = stage_counter.compare_exchange(
                        stage,
                        stage + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    );
                    return Vec::new();
                }
            },
            StageTracking::ExternallySet(_) => {
                if stage >= self.stage_switch_conditions.len()
                    || (stage < self.stage_switch_conditions.len()
                        && self
                            .stage_switch_conditions
                            .get(stage)
                            .unwrap()
                            .should_switch())
                {
                    info!("TransactionGenerator Workflow: Stage {} has consumed all accounts, moving to stage {}", stage, stage + 1);
                    return Vec::new();
                }
            },
        }

        sample!(
            SampleRate::Duration(Duration::from_secs(2)),
            info!("Cur stage: {}, stage switch conditions: {:?}", stage, self.stage_switch_conditions);
        );

        let result = if let Some(generator) = self.generators.get_mut(stage) {
            generator.generate_transactions(account, num_to_create, &Vec::new(), false)
        } else {
            Vec::new()
        };
        if let Some(switch_condition) = self.stage_switch_conditions.get_mut(stage) {
            switch_condition.reduce_txn_count(result.len());
        }
        result
    }
}

#[derive(Clone)]
enum StageSwitchCondition {
    WhenPoolBecomesEmpty(Arc<ObjectPool<LocalAccount>>),
    WhenPoolWithHistoryBecomesEmpty(Arc<ObjectPool<(LocalAccount, Vec<String>)>>),
    MaxTransactions(Arc<AtomicUsize>),
}

impl StageSwitchCondition {
    fn should_switch(&self) -> bool {
        match self {
            StageSwitchCondition::WhenPoolBecomesEmpty(pool) => pool.len() == 0,
            StageSwitchCondition::WhenPoolWithHistoryBecomesEmpty(pool) => pool.len() == 0,
            StageSwitchCondition::MaxTransactions(max) => max.load(Ordering::Relaxed) == 0,
        }
    }

    fn reduce_txn_count(&mut self, count: usize) {
        match self {
            StageSwitchCondition::WhenPoolBecomesEmpty(_) => {},
            StageSwitchCondition::WhenPoolWithHistoryBecomesEmpty(_) => {},
            StageSwitchCondition::MaxTransactions(max) => {
                let current = max.load(Ordering::Relaxed);
                if count > current {
                    max.store(0, Ordering::Relaxed);
                } else {
                    max.fetch_sub(count, Ordering::Relaxed);
                }
            },
        }
    }
}
impl Debug for StageSwitchCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StageSwitchCondition::WhenPoolBecomesEmpty(pool) => {
                write!(f, "WhenPoolBecomesEmpty({})", pool.len())
            },
            StageSwitchCondition::WhenPoolWithHistoryBecomesEmpty(pool) => {
                write!(f, "WhenPoolWithHistoryBecomesEmpty({})", pool.len())
            },
            StageSwitchCondition::MaxTransactions(max) => {
                write!(f, "MaxTransactions({})", max.load(Ordering::Relaxed))
            },
        }
    }
}

pub struct WorkflowTxnGeneratorCreator {
    stage: StageTracking,
    creators: Vec<Box<dyn TransactionGeneratorCreator>>,
    stage_switch_conditions: Vec<StageSwitchCondition>,
}

impl WorkflowTxnGeneratorCreator {
    fn new(
        stage: StageTracking,
        creators: Vec<Box<dyn TransactionGeneratorCreator>>,
        stage_switch_conditions: Vec<StageSwitchCondition>,
    ) -> Self {
        Self {
            stage,
            creators,
            stage_switch_conditions,
        }
    }

    pub async fn create_workload(
        workflow_kind: WorkflowKind,
        txn_factory: TransactionFactory,
        init_txn_factory: TransactionFactory,
        root_account: &dyn RootAccountHandle,
        txn_executor: &dyn ReliableTransactionSubmitter,
        num_modules: usize,
        initial_account_pool: Option<Arc<ObjectPool<LocalAccount>>>,
        cur_phase: Arc<AtomicUsize>,
        progress_type: WorkflowProgress,
    ) -> Self {
        assert_eq!(num_modules, 1, "Only one module is supported for now");

        let stage_tracking = match progress_type {
            WorkflowProgress::MoveByPhases => StageTracking::ExternallySet(cur_phase),
            WorkflowProgress::WhenDone {
                delay_between_stages_s,
            } => StageTracking::WhenDone {
                stage_counter: Arc::new(AtomicUsize::new(0)),
                stage_start_time: Arc::new(AtomicU64::new(0)),
                delay_between_stages: Duration::from_secs(delay_between_stages_s),
            },
        };
        println!(
            "Creating workload with stage tracking: {:?}",
            match &stage_tracking {
                StageTracking::ExternallySet(_) => "ExternallySet",
                StageTracking::WhenDone { .. } => "WhenDone",
            }
        );
        match workflow_kind {
            WorkflowKind::CreateMintBurn {
                count,
                creation_balance,
            } => {
                let created_pool = Arc::new(ObjectPool::new());
                let minted_pool = Arc::new(ObjectPool::new());
                let burnt_pool = Arc::new(ObjectPool::new());

                let mint_entry_point = EntryPoints::TokenV2AmbassadorMint { numbered: false };
                let burn_entry_point = EntryPoints::TokenV2AmbassadorBurn;

                let mut packages = CustomModulesDelegationGeneratorCreator::publish_package(
                    init_txn_factory.clone(),
                    root_account,
                    txn_executor,
                    num_modules,
                    mint_entry_point.package_name(),
                    Some(40_00000000),
                    true,
                )
                .await;

                let mint_worker = CustomModulesDelegationGeneratorCreator::create_worker(
                    init_txn_factory.clone(),
                    root_account,
                    txn_executor,
                    &mut packages,
                    &mut EntryPointTransactionGenerator {
                        entry_point: mint_entry_point,
                    },
                )
                .await;
                let burn_worker = CustomModulesDelegationGeneratorCreator::create_worker(
                    init_txn_factory.clone(),
                    root_account,
                    txn_executor,
                    &mut packages,
                    &mut EntryPointTransactionGenerator {
                        entry_point: burn_entry_point,
                    },
                )
                .await;

                let packages = Arc::new(packages);

                let creators: Vec<Box<dyn TransactionGeneratorCreator>> = vec![
                    Box::new(AccountGeneratorCreator::new(
                        txn_factory.clone(),
                        None,
                        Some(created_pool.clone()),
                        count,
                        creation_balance,
                    )),
                    Box::new(AccountsPoolWrapperCreator::new(
                        Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                            txn_factory.clone(),
                            packages.clone(),
                            mint_worker,
                        )),
                        created_pool.clone(),
                        Some(minted_pool.clone()),
                    )),
                    Box::new(AccountsPoolWrapperCreator::new(
                        Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                            txn_factory.clone(),
                            packages.clone(),
                            burn_worker,
                        )),
                        minted_pool.clone(),
                        Some(burnt_pool.clone()),
                    )),
                ];
                Self::new(stage_tracking, creators, vec![
                    StageSwitchCondition::MaxTransactions(Arc::new(AtomicUsize::new(count))),
                    StageSwitchCondition::WhenPoolBecomesEmpty(created_pool),
                    StageSwitchCondition::WhenPoolBecomesEmpty(minted_pool),
                ])
            },
            WorkflowKind::Econia {
                num_users,
                flow_type,
                num_markets,
                reuse_accounts_for_orders,
                publish_packages,
            } => {
                // let create_accounts = initial_account_pool.is_none();
                let create_accounts = false;
                // info!("Create_accounts {:?}", create_accounts);
                let created_pool = initial_account_pool.unwrap_or(Arc::new(ObjectPool::new()));
                let register_market_accounts_pool = Arc::new(ObjectPool::new());
                let deposit_coins_pool = Arc::new(ObjectPool::new());
                let deposit_coins_pool_with_added_history = Arc::new(ObjectPool::new());
                let place_orders_pool = Arc::new(ObjectPool::new());

                let mut packages = CustomModulesDelegationGeneratorCreator::publish_package(
                    init_txn_factory.clone(),
                    root_account,
                    txn_executor,
                    num_modules,
                    EntryPoints::EconiaRegisterMarket.package_name(),
                    Some(100_000_000_000_000),
                    publish_packages,
                )
                .await;

                if publish_packages {
                    register_econia_markets(
                        init_txn_factory.clone(),
                        &mut packages,
                        txn_executor,
                        num_markets,
                    )
                    .await;
                }

                let econia_register_market_user_worker = match flow_type {
                    EconiaFlowType::Real => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaRegisterMarketUserTransactionGenerator::new(
                                num_markets,
                                false,
                            ),
                        )
                        .await
                    },
                    _ => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaRegisterMarketUserTransactionGenerator::new(
                                num_markets,
                                true,
                            ),
                        )
                        .await
                    },
                };

                let econia_deposit_coins_worker = match flow_type {
                    EconiaFlowType::Real => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaDepositCoinsTransactionGenerator::new(num_markets, false),
                        )
                        .await
                    },
                    _ => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaDepositCoinsTransactionGenerator::new(num_markets, true),
                        )
                        .await
                    },
                };

                let econia_place_orders_worker = match flow_type {
                    EconiaFlowType::Basic => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EntryPointTransactionGenerator {
                                entry_point: EntryPoints::EconiaPlaceRandomLimitOrder,
                            },
                        )
                        .await
                    },
                    EconiaFlowType::Mixed => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaLimitOrderTransactionGenerator::new(
                                num_markets,
                                (num_users as u64) * 2,
                            ),
                        )
                        .await
                    },
                    EconiaFlowType::Market => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaMarketOrderTransactionGenerator::new(
                                num_markets,
                                (num_users as u64) * 2,
                            ),
                        )
                        .await
                    },
                    EconiaFlowType::Real => {
                        CustomModulesDelegationGeneratorCreator::create_worker(
                            init_txn_factory.clone(),
                            root_account,
                            txn_executor,
                            &mut packages,
                            &mut EconiaRealOrderTransactionGenerator::default(),
                        )
                        .await
                    },
                };

                let packages = Arc::new(packages);

                let mut creators: Vec<Box<dyn TransactionGeneratorCreator>> = vec![];
                let mut stage_switch_conditions = vec![];
                if create_accounts {
                    creators.push(Box::new(AccountGeneratorCreator::new(
                        txn_factory.clone(),
                        None,
                        Some(created_pool.clone()),
                        num_users,
                        400_000_000,
                    )));
                    stage_switch_conditions.push(StageSwitchCondition::MaxTransactions(
                        Arc::new(AtomicUsize::new(num_users)),
                    ));
                }

                creators.push(Box::new(AccountsPoolWrapperCreator::new(
                    Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                        txn_factory.clone(),
                        packages.clone(),
                        econia_register_market_user_worker,
                    )),
                    created_pool.clone(),
                    Some(register_market_accounts_pool.clone()),
                )));
                stage_switch_conditions.push(StageSwitchCondition::WhenPoolBecomesEmpty(
                    created_pool.clone(),
                ));

                creators.push(Box::new(AccountsPoolWrapperCreator::new(
                    Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                        txn_factory.clone(),
                        packages.clone(),
                        econia_deposit_coins_worker,
                    )),
                    register_market_accounts_pool.clone(),
                    Some(deposit_coins_pool.clone()),
                )));
                stage_switch_conditions.push(StageSwitchCondition::WhenPoolBecomesEmpty(
                    register_market_accounts_pool.clone(),
                ));

                if flow_type == EconiaFlowType::Real {
                    creators.push(Box::new(AddHistoryWrapperCreator::new(
                        deposit_coins_pool.clone(),
                        deposit_coins_pool_with_added_history.clone(),
                    )));
                    stage_switch_conditions.push(StageSwitchCondition::WhenPoolBecomesEmpty(
                        deposit_coins_pool.clone(),
                    ));
                    creators.push(Box::new(MarketMakerPoolWrapperCreator::new(
                        Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                            txn_factory.clone(),
                            packages.clone(),
                            econia_place_orders_worker,
                        )),
                        deposit_coins_pool_with_added_history.clone(),
                    )));
                    stage_switch_conditions.push(StageSwitchCondition::MaxTransactions(
                        Arc::new(AtomicUsize::new(2_000_000)),
                    ));
                } else if reuse_accounts_for_orders {
                    creators.push(Box::new(ReuseAccountsPoolWrapperCreator::new(
                        Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                            txn_factory.clone(),
                            packages.clone(),
                            econia_place_orders_worker,
                        )),
                        deposit_coins_pool.clone(),
                    )));
                    stage_switch_conditions.push(StageSwitchCondition::MaxTransactions(
                        Arc::new(AtomicUsize::new(2_000_000)),
                    ));
                } else {
                    creators.push(Box::new(AccountsPoolWrapperCreator::new(
                        Box::new(CustomModulesDelegationGeneratorCreator::new_raw(
                            txn_factory.clone(),
                            packages.clone(),
                            econia_place_orders_worker,
                        )),
                        deposit_coins_pool.clone(),
                        Some(place_orders_pool.clone()),
                    )));
                    stage_switch_conditions.push(StageSwitchCondition::WhenPoolBecomesEmpty(
                        deposit_coins_pool.clone(),
                    ));
                }

                // let mut pool_per_stage = Vec::new();
                // if create_accounts {
                //     pool_per_stage.push(Pool::AccountPool(created_pool));
                // }
                // pool_per_stage.push(Pool::AccountPool(register_market_accounts_pool));
                // pool_per_stage.push(Pool::AccountPool(deposit_coins_pool));
                // if flow_type == EconiaFlowType::Real {
                //     pool_per_stage.push(Pool::AccountWithHistoryPool(
                //         deposit_coins_pool_with_added_history,
                //     ));
                // }
                // pool_per_stage.push(Pool::AccountPool(place_orders_pool));
                // let pool_per_stage = if create_accounts {
                //     vec![
                //         created_pool,
                //         register_market_accounts_pool,
                //         deposit_coins_pool,
                //         place_orders_pool,
                //     ]
                // } else {
                //     vec![
                //         register_market_accounts_pool,
                //         deposit_coins_pool,
                //         place_orders_pool,
                //     ]
                // };
                Self::new(stage_tracking, creators, stage_switch_conditions)
            },
        }
    }
}

impl TransactionGeneratorCreator for WorkflowTxnGeneratorCreator {
    fn create_transaction_generator(
        &self,
        txn_counter: Arc<AtomicU64>,
    ) -> Box<dyn TransactionGenerator> {
        Box::new(WorkflowTxnGenerator::new(
            self.stage.clone(),
            self.creators
                .iter()
                .map(|c| c.create_transaction_generator(txn_counter.clone()))
                .collect(),
            self.stage_switch_conditions.clone(),
        ))
    }
}
