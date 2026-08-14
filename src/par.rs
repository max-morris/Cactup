//! A fixed-width parallel map for per-repo filesystem walks (probes, status
//! walks): each one is stat-latency-bound on the network filesystems clusters
//! keep source trees on, so walking ~80 repos sequentially multiplies that
//! latency by the repo count. Network fetches have their own pool
//! (`fetch::WORKERS`); this one is for local(ish) disk walks only.

use crate::Res;
use anyhow::bail;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Worker width: enough concurrency to hide per-stat round trips, capped so
/// a wide login node does not aim dozens of concurrent walks at a shared
/// filesystem (and gix's status may fan out threads of its own per walk).
fn workers_for(items: usize) -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8).min(items.max(1))
}

/// Map `f` over `items` on a [`workers_for`]-wide pool, returning results in
/// input order. `f` runs on worker threads — anything shared (a progress
/// item, a cache) must sit behind a lock. Honors Ctrl-C: workers stop
/// pulling items once `gix::interrupt::is_triggered()`, and the whole map
/// then fails with "interrupted" rather than returning partial results.
pub(crate) fn parallel_map<T: Sync, R: Send>(
    items: &[T],
    f: impl Fn(&T) -> R + Sync,
) -> Res<Vec<R>> {
    let slots: Vec<Mutex<Option<R>>> = items.iter().map(|_| Mutex::new(None)).collect();
    // Relaxed suffices: the dispenser only needs RMW atomicity (nothing is
    // published through it), and the collecting loop below is ordered after
    // every slot write by scope's join of all workers.
    let next = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        for _ in 0..workers_for(items.len()) {
            scope.spawn(|| loop {
                if gix::interrupt::is_triggered() {
                    return;
                }
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
    if gix::interrupt::is_triggered() {
        bail!("interrupted");
    }
    Ok(slots
        .into_iter()
        .map(|s| s.into_inner().expect("parallel_map slot poisoned").expect("worker filled slot"))
        .collect())
}

/// Join stream-copier threads, giving up after `grace`: a pipe only hits EOF
/// once *every* holder of its write end closes it, so a child that
/// backgrounded a process with our pipes still attached would otherwise hang
/// the join long after the child itself exited. Buffered bytes drain in
/// milliseconds; only a live background writer keeps a copier alive past the
/// deadline, and then `leak_note` is printed once and the threads are
/// abandoned (they exit with the process).
pub(crate) fn join_with_deadline(
    handles: impl IntoIterator<Item = std::thread::JoinHandle<()>>,
    grace: Duration,
    leak_note: &str,
) {
    let deadline = Instant::now() + grace;
    let mut noted = false;
    for handle in handles {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
        }
        if handle.is_finished() {
            let _ = handle.join();
        } else if !noted {
            noted = true;
            eprintln!("{leak_note}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_input_order_and_covers_every_item() {
        let items: Vec<usize> = (0..100).collect();
        assert_eq!(
            parallel_map(&items, |&i| i * 2).unwrap(),
            (0..100).map(|i| i * 2).collect::<Vec<_>>()
        );
        let empty: Vec<usize> = parallel_map(&[], |_: &usize| 0).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn join_with_deadline_abandons_a_stuck_thread() {
        let (never_tx, never_rx) = std::sync::mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || {
            let _ = never_rx.recv(); // blocks until the sender drops at test end
        });
        let quick = std::thread::spawn(|| {});
        let start = Instant::now();
        join_with_deadline([quick, stuck], Duration::from_millis(100), "note: leaked");
        assert!(start.elapsed() < Duration::from_secs(2), "must not wait for the stuck thread");
        drop(never_tx);
    }
}
