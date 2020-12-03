use crate::cases::Metric;
use crate::stats::Measurements;
use crate::testbed::RuntimeTestbed;
use indicatif::{ProgressBar, ProgressStyle};
use near_crypto::{InMemorySigner, KeyType};
use near_primitives::hash::CryptoHash;
use near_primitives::transaction::{Action, SignedTransaction};
use near_vm_logic::VMKind;
use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::convert::TryInto;
use std::path::PathBuf;
use std::time::Instant;
use std::{fs::File, io::Read, os::unix::io::FromRawFd};

/// Get account id from its index.
pub fn get_account_id(account_index: usize) -> String {
    format!("near_{}_{}", account_index, account_index)
}

/// Total number of transactions that we need to prepare.
pub fn total_transactions(config: &Config) -> usize {
    config.block_sizes.iter().sum::<usize>() * config.iter_per_block
}

fn warmup_total_transactions(config: &Config) -> usize {
    config.block_sizes.iter().sum::<usize>() * config.warmup_iters_per_block
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GasMetric {
    // If we measure gas in number of executed instructions, must run under simulator.
    ICount,
    // If we measure gas in elapsed time.
    Time,
}

/// Configuration which we use to run measurements.
#[derive(Debug, Clone)]
pub struct Config {
    /// How many warm up iterations per block should we run.
    pub warmup_iters_per_block: usize,
    /// How many iterations per block are we going to try.
    pub iter_per_block: usize,
    /// Total active accounts.
    pub active_accounts: usize,
    /// Number of the transactions in the block.
    pub block_sizes: Vec<usize>,
    /// Where state dump is located in case we need to create a testbed.
    pub state_dump_path: String,
    /// Metric used for counting.
    pub metric: GasMetric,
    /// VMKind used
    pub vm_kind: VMKind,
    /// Whether to measure ActionCreationConfig
    pub disable_measure_action_creation: bool,
    /// Whether to measure Transaction
    pub disable_measure_transaction: bool,
}

/// Measure the speed of transactions containing certain simple actions.
pub fn measure_actions(
    metric: Metric,
    measurements: &mut Measurements,
    config: &Config,
    testbed: Option<RuntimeTestbed>,
    actions: Vec<Action>,
    sender_is_receiver: bool,
    use_unique_accounts: bool,
) -> RuntimeTestbed {
    let mut nonces: HashMap<usize, u64> = HashMap::new();
    let mut accounts_used = HashSet::new();
    let mut f = || {
        let account_idx = loop {
            let x = rand::thread_rng().gen::<usize>() % config.active_accounts;
            if use_unique_accounts && accounts_used.contains(&x) {
                continue;
            }
            break x;
        };
        let other_account_idx = loop {
            if sender_is_receiver {
                break account_idx;
            }
            let x = rand::thread_rng().gen::<usize>() % config.active_accounts;
            if use_unique_accounts && accounts_used.contains(&x) || x == account_idx {
                continue;
            }
            break x;
        };
        accounts_used.insert(account_idx);
        accounts_used.insert(other_account_idx);
        let account_id = get_account_id(account_idx);
        let other_account_id = get_account_id(other_account_idx);

        let signer = InMemorySigner::from_seed(&account_id, KeyType::ED25519, &account_id);
        let nonce = *nonces.entry(account_idx).and_modify(|x| *x += 1).or_insert(1);

        SignedTransaction::from_actions(
            nonce as u64,
            account_id,
            other_account_id,
            &signer,
            actions.clone(),
            CryptoHash::default(),
        )
    };
    measure_transactions(metric, measurements, config, testbed, &mut f, false)
}

// TODO: super-ugly, can achieve the same via higher-level wrappers over POSIX read().
#[cfg(target_family = "unix")]
#[inline(always)]
pub unsafe fn syscall3(fd: u32, buf: &mut [u8]) {
    let mut f = File::from_raw_fd(std::mem::transmute::<u32, i32>(fd));
    let _ = f.read(buf);
    std::mem::forget(f); // Skips closing the file descriptor, but throw away reference
}

const CATCH_BASE: u32 = 0xcafebabe;

pub enum Consumed {
    Instant(Instant),
    None,
}

fn start_count_instructions() -> Consumed {
    let mut buf: i8 = 0;
    unsafe {
        syscall3(CATCH_BASE, std::mem::transmute::<*mut i8, &mut [u8; 1]>(&mut buf));
    }
    Consumed::None
}

fn end_count_instructions() -> u64 {
    let mut result: u64 = 0;
    unsafe {
        syscall3(CATCH_BASE + 1, std::mem::transmute::<*mut u64, &mut [u8; 8]>(&mut result));
    }
    result
}

fn start_count_time() -> Consumed {
    Consumed::Instant(Instant::now())
}

fn end_count_time(consumed: &Consumed) -> u64 {
    match *consumed {
        Consumed::Instant(instant) => instant.elapsed().as_nanos().try_into().unwrap(),
        Consumed::None => panic!("Must not be so"),
    }
}

pub fn start_count(metric: GasMetric) -> Consumed {
    return match metric {
        GasMetric::ICount => start_count_instructions(),
        GasMetric::Time => start_count_time(),
    };
}

pub fn end_count(metric: GasMetric, consumed: &Consumed) -> u64 {
    return match metric {
        GasMetric::ICount => end_count_instructions(),
        GasMetric::Time => end_count_time(consumed),
    };
}

/// Measure the speed of the transactions, given a transactions-generator function.
/// Returns testbed so that it can be reused.
pub fn measure_transactions<F>(
    metric: Metric,
    measurements: &mut Measurements,
    config: &Config,
    testbed: Option<RuntimeTestbed>,
    f: &mut F,
    allow_failures: bool,
) -> RuntimeTestbed
where
    F: FnMut() -> SignedTransaction,
{
    let mut testbed = match testbed {
        Some(x) => {
            println!("{:?}. Reusing testbed.", metric);
            x
        }
        None => {
            let path = PathBuf::from(config.state_dump_path.as_str());
            println!("{:?}. Preparing testbed. Loading state.", metric);
            RuntimeTestbed::from_state_dump(&path)
        }
    };

    if config.warmup_iters_per_block > 0 {
        let bar = ProgressBar::new(warmup_total_transactions(config) as _);
        bar.set_style(ProgressStyle::default_bar().template(
            "[elapsed {elapsed_precise} remaining {eta_precise}] Warm up {bar} {pos:>7}/{len:7} {msg}",
        ));
        for block_size in config.block_sizes.clone() {
            for _ in 0..config.warmup_iters_per_block {
                let block: Vec<_> = (0..block_size).map(|_| (*f)()).collect();
                testbed.process_block(&block, allow_failures);
                bar.inc(block_size as _);
                bar.set_message(format!("Block size: {}", block_size).as_str());
            }
        }
        testbed.process_blocks_until_no_receipts(allow_failures);
        bar.finish();
    }

    let bar = ProgressBar::new(total_transactions(config) as _);
    bar.set_style(ProgressStyle::default_bar().template(
        "[elapsed {elapsed_precise} remaining {eta_precise}] Measuring {bar} {pos:>7}/{len:7} {msg}",
    ));
    node_runtime::EXT_COSTS_COUNTER.with(|f| {
        f.borrow_mut().clear();
    });
    for _ in 0..config.iter_per_block {
        for block_size in config.block_sizes.clone() {
            let block: Vec<_> = (0..block_size).map(|_| (*f)()).collect();
            let start = start_count(config.metric);
            testbed.process_block(&block, allow_failures);
            testbed.process_blocks_until_no_receipts(allow_failures);
            let measured = end_count(config.metric, &start);
            measurements.record_measurement(metric.clone(), block_size, measured);
            bar.inc(block_size as _);
            bar.set_message(format!("Block size: {}", block_size).as_str());
        }
    }
    bar.finish();
    measurements.print();
    testbed
}
