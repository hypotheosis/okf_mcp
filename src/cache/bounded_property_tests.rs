//! Property test of [`Bounded`]'s invalidation epochs and fence (M18 spec,
//! Testing; issue #744).
//!
//! A generated schedule interleaves `get_or_load`, `get_or_revalidate`,
//! `invalidate`, `invalidate_where`, `reweigh` and `mark_all_stale` against a
//! model "database" (one version number per key). The test, not a clock,
//! decides when each load finishes: every loader reads the database when it
//! starts, then parks on a gate that only a `Release` step opens, and the
//! operation futures are polled by hand (with a waker that does nothing), one
//! `Poll` step at a time. No timer, sleep or thread is involved, so a failure
//! replays exactly and hegel can shrink the schedule.
//!
//! What the schedule can interleave is the points where a loader is parked.
//! The windows inside the cache after a loader returns (insert, then the
//! second `current()` check) run to completion within one poll and stay
//! covered by the deterministic tests in `bounded.rs`.
//!
//! # For Python readers
//!
//! A Rust future does nothing until it is polled. Polling one by hand, like
//! calling `next()` on a Python generator, runs it to its next `.await` that
//! cannot finish yet (`Poll::Pending`) or to its end (`Poll::Ready`).

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use super::*;

/// Keys the schedule uses: few, so operations collide.
const KEYS: u8 = 4;
/// The cache budget in weight units.
const BUDGET: u64 = 100;
/// The heaviest value: above `BUDGET`, so some values are served, not kept.
const MAX_WEIGHT: u32 = 150;
/// Most steps in one schedule.
const MAX_STEPS: usize = 60;
/// Most poll rounds the final drain may take before the test calls it a hang.
const DRAIN_ROUNDS: usize = 1_000;

/// The model database: a version per key (what a load reads) and a weight
/// per key (what the weigher reports).
struct World {
    versions: [AtomicU64; KEYS as usize],
    weights: [AtomicU32; KEYS as usize],
}

impl World {
    fn new() -> Self {
        Self {
            versions: std::array::from_fn(|_| AtomicU64::new(1)),
            weights: std::array::from_fn(|_| AtomicU32::new(1)),
        }
    }

    fn version(&self, key: u8) -> u64 {
        self.versions[usize::from(key)].load(Ordering::SeqCst)
    }

    /// A change in the database: loads that read the old version are stale.
    fn bump(&self, key: u8) {
        self.versions[usize::from(key)].fetch_add(1, Ordering::SeqCst);
    }

    fn weight(&self, key: u8) -> u32 {
        self.weights[usize::from(key)].load(Ordering::SeqCst)
    }
}

/// One generated action.
#[derive(Debug, Clone, Copy)]
enum Step {
    /// Start `get_or_load(key)` and poll it once.
    Load { key: u8 },
    /// Start `get_or_revalidate(key)` and poll it once.
    Revalidate { key: u8 },
    /// Open the gate of pending operation `index % pending`.
    Release { index: usize },
    /// Poll pending operation `index % pending` once.
    Poll { index: usize },
    /// Drop pending operation `index % pending`, as a cancelled request.
    Cancel { index: usize },
    /// The database changed `key`; tell the cache.
    Invalidate { key: u8 },
    /// The database changed every key with `key % modulus == remainder`;
    /// tell the cache by predicate.
    InvalidateWhere { modulus: u8, remainder: u8 },
    /// `key`'s value now weighs `weight`; tell the cache.
    Reweigh { key: u8, weight: u32 },
    /// A notification connection was lost.
    MarkAllStale,
}

type OpFuture<'a> = Pin<Box<dyn Future<Output = Result<u64, OkfError>> + 'a>>;

/// An operation that has not finished: its future and the gate its loader
/// (or check) waits on.
struct Pending<'a> {
    fut: OpFuture<'a>,
    gate: Arc<AtomicBool>,
    key: u8,
}

/// Pending until `gate` is opened. Nothing wakes it: the schedule polls.
async fn parked(gate: Arc<AtomicBool>) {
    poll_fn(|_| {
        if gate.load(Ordering::SeqCst) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

/// Polls `fut` once with a waker that does nothing.
fn poll_once(fut: &mut OpFuture<'_>) -> Poll<Result<u64, OkfError>> {
    fut.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// Runs one schedule, asserting the properties after every step.
async fn run(cache: &Bounded<u8, u64>, world: &Arc<World>, steps: &[Step]) {
    let mut pending: Vec<Pending<'_>> = Vec::new();
    for step in steps {
        match *step {
            Step::Load { key } => {
                let gate = Arc::new(AtomicBool::new(false));
                let (read_from, gated) = (world.clone(), gate.clone());
                let fut = cache.get_or_load(key, move || async move {
                    // The loader reads the database when it starts, like a
                    // query that took its snapshot before it waits for rows.
                    let version = read_from.version(key);
                    parked(gated).await;
                    Ok(version)
                });
                start(&mut pending, Box::pin(fut), gate, key, world);
            }
            Step::Revalidate { key } => {
                let gate = Arc::new(AtomicBool::new(false));
                let (read_from, gated) = (world.clone(), gate.clone());
                let (check_from, check_gate) = (world.clone(), gate.clone());
                let fut = cache.get_or_revalidate(
                    key,
                    move || async move {
                        let version = read_from.version(key);
                        parked(gated).await;
                        Ok(version)
                    },
                    move |held| async move {
                        let version = check_from.version(key);
                        parked(check_gate).await;
                        Ok((version != held).then_some(version))
                    },
                );
                start(&mut pending, Box::pin(fut), gate, key, world);
            }
            Step::Release { index } => {
                if !pending.is_empty() {
                    pending[index % pending.len()]
                        .gate
                        .store(true, Ordering::SeqCst);
                }
            }
            Step::Poll { index } => {
                if !pending.is_empty() {
                    let at = index % pending.len();
                    if let Poll::Ready(done) = poll_once(&mut pending[at].fut) {
                        done.expect("the loader never fails");
                        pending.remove(at);
                    }
                }
            }
            Step::Cancel { index } => {
                if !pending.is_empty() {
                    pending.remove(index % pending.len());
                }
            }
            Step::Invalidate { key } => {
                world.bump(key);
                cache.invalidate(&key).await;
            }
            Step::InvalidateWhere { modulus, remainder } => {
                (0..KEYS)
                    .filter(|key| key % modulus == remainder)
                    .for_each(|key| world.bump(key));
                cache
                    .invalidate_where(|key, _| key % modulus == remainder)
                    .await;
            }
            Step::Reweigh { key, weight } => {
                world.weights[usize::from(key)].store(weight, Ordering::SeqCst);
                cache.reweigh(&key).await;
            }
            Step::MarkAllStale => cache.mark_all_stale(),
        }
        check_properties(cache, world, &format!("after {step:?}")).await;
    }
    drain(cache, world, pending).await;
}

/// Polls a new operation once. If it finishes at once the cache answered it
/// without loading (every loader is parked), so the value must be current.
fn start<'a>(
    pending: &mut Vec<Pending<'a>>,
    mut fut: OpFuture<'a>,
    gate: Arc<AtomicBool>,
    key: u8,
    world: &World,
) {
    match poll_once(&mut fut) {
        Poll::Ready(served) => assert_eq!(
            served.expect("the loader never fails"),
            world.version(key),
            "key {key}: a lookup answered from the cache served a value from before an \
             invalidation"
        ),
        Poll::Pending => pending.push(Pending { fut, gate, key }),
    }
}

/// Opens every gate and polls until every operation finishes.
async fn drain(cache: &Bounded<u8, u64>, world: &World, mut pending: Vec<Pending<'_>>) {
    for pending in &pending {
        pending.gate.store(true, Ordering::SeqCst);
    }
    for _ in 0..DRAIN_ROUNDS {
        if pending.is_empty() {
            break;
        }
        pending.retain_mut(|op| match poll_once(&mut op.fut) {
            Poll::Ready(done) => {
                done.expect("the loader never fails");
                false
            }
            Poll::Pending => true,
        });
    }
    assert!(
        pending.is_empty(),
        "operations still pending for keys {:?} after {DRAIN_ROUNDS} poll rounds",
        pending.iter().map(|op| op.key).collect::<Vec<_>>()
    );
    check_properties(cache, world, "after the drain").await;
    assert_eq!(
        cache.tracked_epochs(),
        0,
        "no load in flight, so no epoch may remain"
    );
}

/// The two properties, checked between steps.
async fn check_properties(cache: &Bounded<u8, u64>, world: &World, when: &str) {
    cache.settle().await;
    let mut resident_weight = 0_u64;
    for key in 0..KEYS {
        if let Some(held) = cache.peek(&key).await {
            // Property 1: nothing loaded before the last invalidation of its
            // key (or before a fence move that named it) is still cached.
            assert_eq!(
                held,
                world.version(key),
                "key {key} holds a value loaded before its invalidation, {when}"
            );
            resident_weight += u64::from(world.weight(key));
        }
    }
    // Property 2: total weight stays within the budget, by moka's account
    // and by the model's (weights only change together with a `reweigh`).
    let counted = cache.cache.weighted_size();
    assert!(counted <= BUDGET, "weight {counted} over {BUDGET}, {when}");
    assert!(
        resident_weight <= BUDGET,
        "resident keys weigh {resident_weight} over {BUDGET}, {when}"
    );
}

/// Draws one step.
fn draw_step(tc: &hegel::TestCase) -> Step {
    use hegel::generators::integers;
    let key = || tc.draw_silent(integers::<u8>().max_value(KEYS - 1));
    let index = || tc.draw_silent(integers::<usize>().max_value(MAX_STEPS));
    match tc.draw_silent(integers::<u8>().max_value(8)) {
        0 => Step::Load { key: key() },
        1 => Step::Revalidate { key: key() },
        2 => Step::Release { index: index() },
        3 => Step::Poll { index: index() },
        4 => Step::Cancel { index: index() },
        5 => Step::Invalidate { key: key() },
        6 => {
            let modulus = tc.draw_silent(integers::<u8>().min_value(1).max_value(3));
            let remainder = tc.draw_silent(integers::<u8>().max_value(modulus - 1));
            Step::InvalidateWhere { modulus, remainder }
        }
        7 => Step::Reweigh {
            key: key(),
            weight: tc.draw_silent(integers::<u32>().min_value(1).max_value(MAX_WEIGHT)),
        },
        _ => Step::MarkAllStale,
    }
}

/// Property: under any interleaving of loads, revalidations, invalidations,
/// predicate invalidations (the fence), reweighs and stale marking, no cached
/// value predates an invalidation of its key, and the cached weight never
/// exceeds the budget. 1000 cases of up to 60 steps run in about a second.
#[hegel::test(test_cases = 1000, derandomize = true)]
fn loads_overtaken_by_an_invalidation_are_never_cached_and_weight_stays_in_budget(
    tc: hegel::TestCase,
) {
    let count = tc.draw_silent(hegel::generators::integers::<usize>().max_value(MAX_STEPS));
    let steps: Vec<Step> = (0..count).map(|_| draw_step(&tc)).collect();
    tc.note(&format!("{steps:#?}"));

    let world = Arc::new(World::new());
    let weigher_world = world.clone();
    let cache = Bounded::<u8, u64>::new(
        CacheName::Directory,
        Limit::Bytes(BUDGET),
        Duration::from_secs(3600),
    )
    .with_weigher(move |key, _| u64::from(weigher_world.weight(*key)));

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime builds")
        .block_on(run(&cache, &world, &steps));
}
