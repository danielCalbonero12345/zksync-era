use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use backon::{ConstantBuilder, Retryable};
use multivm::interface::{L1BatchEnv, L2BlockEnv, SystemEnv, TxExecutionMode};
use tokio::{
    sync::{watch, RwLock},
    task::JoinHandle,
};
use zksync_contracts::{BaseSystemContracts, SystemContractCode};
use zksync_core::state_keeper::{StateKeeperOutputHandler, UpdatesManager};
use zksync_dal::{ConnectionPool, Core};
use zksync_types::L1BatchNumber;

use crate::tests::IoMock;
use crate::{ConcurrentOutputHandlerFactory, OutputHandlerFactory};

#[derive(Debug)]
struct TestOutputFactory {
    delays: HashMap<L1BatchNumber, Duration>,
}

#[async_trait]
impl OutputHandlerFactory for TestOutputFactory {
    async fn create_handler(
        &mut self,
        l1_batch_number: L1BatchNumber,
    ) -> anyhow::Result<Box<dyn StateKeeperOutputHandler>> {
        let delay = self.delays.get(&l1_batch_number).copied();
        #[derive(Debug)]
        struct TestOutputHandler {
            delay: Option<Duration>,
        }
        #[async_trait]
        impl StateKeeperOutputHandler for TestOutputHandler {
            async fn handle_l2_block(
                &mut self,
                _updates_manager: &UpdatesManager,
            ) -> anyhow::Result<()> {
                Ok(())
            }

            async fn handle_l1_batch(
                &mut self,
                _updates_manager: Arc<UpdatesManager>,
            ) -> anyhow::Result<()> {
                if let Some(delay) = self.delay {
                    tokio::time::sleep(delay).await
                }
                Ok(())
            }
        }
        Ok(Box::new(TestOutputHandler { delay }))
    }
}

struct OutputHandlerTester {
    io: Arc<RwLock<IoMock>>,
    output_factory: ConcurrentOutputHandlerFactory<Arc<RwLock<IoMock>>, TestOutputFactory>,
    tasks: Vec<JoinHandle<()>>,
    stop_sender: watch::Sender<bool>,
}

impl OutputHandlerTester {
    fn new(
        io: Arc<RwLock<IoMock>>,
        pool: ConnectionPool<Core>,
        delays: HashMap<L1BatchNumber, Duration>,
    ) -> Self {
        let test_factory = TestOutputFactory { delays };
        let (output_factory, task) =
            ConcurrentOutputHandlerFactory::new(pool, io.clone(), test_factory);
        let (stop_sender, stop_receiver) = watch::channel(false);
        let join_handle = tokio::task::spawn(async move { task.run(stop_receiver).await.unwrap() });
        let tasks = vec![join_handle];
        Self {
            io,
            output_factory,
            tasks,
            stop_sender,
        }
    }

    async fn spawn_test_task(&mut self, l1_batch_number: L1BatchNumber) -> anyhow::Result<()> {
        let mut output_handler = self.output_factory.create_handler(l1_batch_number).await?;
        let join_handle = tokio::task::spawn(async move {
            let l1_batch_env = L1BatchEnv {
                previous_batch_hash: None,
                number: Default::default(),
                timestamp: 0,
                fee_input: Default::default(),
                fee_account: Default::default(),
                enforced_base_fee: None,
                first_l2_block: L2BlockEnv {
                    number: 0,
                    timestamp: 0,
                    prev_block_hash: Default::default(),
                    max_virtual_blocks_to_create: 0,
                },
            };
            let system_env = SystemEnv {
                zk_porter_available: false,
                version: Default::default(),
                base_system_smart_contracts: BaseSystemContracts {
                    bootloader: SystemContractCode {
                        code: vec![],
                        hash: Default::default(),
                    },
                    default_aa: SystemContractCode {
                        code: vec![],
                        hash: Default::default(),
                    },
                },
                bootloader_gas_limit: 0,
                execution_mode: TxExecutionMode::VerifyExecute,
                default_validation_computational_gas_limit: 0,
                chain_id: Default::default(),
            };
            let updates_manager = UpdatesManager::new(&l1_batch_env, &system_env);
            output_handler
                .handle_l2_block(&updates_manager)
                .await
                .unwrap();
            output_handler
                .handle_l1_batch(Arc::new(updates_manager))
                .await
                .unwrap();
        });
        self.tasks.push(join_handle);
        Ok(())
    }

    async fn wait_for_batch(
        &self,
        l1_batch_number: L1BatchNumber,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        const RETRY_INTERVAL: Duration = Duration::from_millis(500);

        let max_tries = (timeout.as_secs_f64() / RETRY_INTERVAL.as_secs_f64()).ceil() as u64;
        (|| async {
            let current = self.io.read().await.current;
            anyhow::ensure!(
                current == l1_batch_number,
                "Batch #{} has not been processed yet (current is #{})",
                l1_batch_number,
                current
            );
            Ok(())
        })
        .retry(
            &ConstantBuilder::default()
                .with_delay(RETRY_INTERVAL)
                .with_max_times(max_tries as usize),
        )
        .await
    }

    async fn wait_for_batch_progressively(
        &self,
        l1_batch_number: L1BatchNumber,
        timeout: Duration,
    ) -> anyhow::Result<()> {
        const SLEEP_INTERVAL: Duration = Duration::from_millis(500);

        let mut current = self.io.read().await.current;
        let max_tries = (timeout.as_secs_f64() / SLEEP_INTERVAL.as_secs_f64()).ceil() as u64;
        let mut try_num = 0;
        loop {
            tokio::time::sleep(SLEEP_INTERVAL).await;
            try_num += 1;
            if try_num >= max_tries {
                anyhow::bail!("Timeout");
            }
            let new_current = self.io.read().await.current;
            // Ensure we did not go back in latest processed batch
            if new_current < current {
                anyhow::bail!(
                    "Latest processed batch regressed to #{} back from #{}",
                    new_current,
                    current
                );
            }
            current = new_current;
            if current >= l1_batch_number {
                return Ok(());
            }
        }
    }

    async fn stop_and_wait_for_all_tasks(self) -> anyhow::Result<()> {
        self.stop_sender.send(true)?;
        futures::future::join_all(self.tasks).await;
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 10)]
async fn monotonically_progress_processed_batches() -> anyhow::Result<()> {
    let pool = ConnectionPool::<Core>::test_pool().await;
    let io = Arc::new(RwLock::new(IoMock {
        current: 0.into(),
        max: 10,
    }));
    // Distribute progressively higher delays for higher batches so that we can observe
    // each batch being marked as processed. In other words, batch 1 would be marked as processed,
    // then there will be a minimum 1 sec of delay (more in <10 thread environments), then batch
    // 2 would be marked as processed etc.
    let delays = (1..10)
        .map(|i| (L1BatchNumber(i), Duration::from_secs(i as u64)))
        .collect();
    let mut tester = OutputHandlerTester::new(io.clone(), pool, delays);
    for i in 1..10 {
        tester.spawn_test_task(i.into()).await?;
    }
    assert_eq!(io.read().await.current, L1BatchNumber(0));
    for i in 1..10 {
        tester
            .wait_for_batch(i.into(), Duration::from_secs(10))
            .await?;
    }
    tester.stop_and_wait_for_all_tasks().await?;
    assert_eq!(io.read().await.current, L1BatchNumber(9));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 10)]
async fn do_not_progress_with_gaps() -> anyhow::Result<()> {
    let pool = ConnectionPool::<Core>::test_pool().await;
    let io = Arc::new(RwLock::new(IoMock {
        current: 0.into(),
        max: 10,
    }));
    // Distribute progressively lower delays for higher batches so that we can observe last
    // processed batch not move until the first batch (with longest delay) is processed.
    let delays = (1..10)
        .map(|i| (L1BatchNumber(i), Duration::from_secs(10 - i as u64)))
        .collect();
    let mut tester = OutputHandlerTester::new(io.clone(), pool, delays);
    for i in 1..10 {
        tester.spawn_test_task(i.into()).await?;
    }
    assert_eq!(io.read().await.current, L1BatchNumber(0));
    tester
        .wait_for_batch_progressively(L1BatchNumber(9), Duration::from_secs(60))
        .await?;
    tester.stop_and_wait_for_all_tasks().await?;
    assert_eq!(io.read().await.current, L1BatchNumber(9));
    Ok(())
}
