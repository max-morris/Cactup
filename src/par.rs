//! A fixed-width parallel map for per-repo filesystem walks (probes, status
//! walks): each one is stat-latency-bound on the network filesystems clusters
//! keep source trees on, so walking ~80 repos sequentially multiplies that
//! latency by the repo count. Network fetches have their own pool
//! (`fetch::WORKERS`); this one is for local(ish) disk walks only.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

/// Worker width: enough concurrency to hide per-stat round trips, capped so
/// a wide login node does not aim dozens of concurrent walks at a shared
/// filesystem (and gix's status may fan out threads of its own per walk).
fn workers_for(items: usize) -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8).min(items.max(1))
}

/// Map `f` over `items` on a [`workers_for`]-wide pool, returning results in
/// input order. `f` runs on worker threads — anything shared (a progress
/// item, a cache) must sit behind a lock.
pub(crate) fn parallel_map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    // Relaxed suffices: the dispenser only needs RMW atomicity (nothing is
    // published through it), and the collecting loop below is ordered after
    // every slot write by scope's join of all workers.
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers_for(items.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { return };
                // Run `f` before taking the slot lock, so a panicking `f`
                // can never poison it and the panic resurfaces verbatim at
                // scope join rather than as a misleading poison expect.
                let result = f(item);
                *slots[i].lock().expect("parallel_map slot poisoned") = Some(result);
            });
        }
    });
    slots
        .into_iter()
        .map(|s| s.into_inner().expect("parallel_map slot poisoned").expect("worker filled slot"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_input_order_and_covers_every_item() {
        let items: Vec<usize> = (0..100).collect();
        assert_eq!(parallel_map(&items, |&i| i * 2), (0..100).map(|i| i * 2).collect::<Vec<_>>());
        let empty: Vec<usize> = parallel_map(&[], |_: &usize| 0);
        assert!(empty.is_empty());
    }
}
