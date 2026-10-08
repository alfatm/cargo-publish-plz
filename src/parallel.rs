//! A tiny scoped worker pool: network and `cargo`/`git` calls per package run concurrently.

use std::num::NonZero;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};

/// Most of the work is waiting on HTTP and subprocesses, so use more threads than cores.
const MIN_THREADS: usize = 8;

/// `items.iter().map(f).collect()`, with `f` running on several threads. Order is kept.
pub fn map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let cores = std::thread::available_parallelism().map_or(1, NonZero::get);
    let threads = cores.max(MIN_THREADS).min(items.len());
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());

    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                let mut index = next.fetch_add(1, Ordering::Relaxed);
                while let Some(item) = items.get(index) {
                    let result = f(item);
                    let mut results = results.lock().unwrap_or_else(PoisonError::into_inner);
                    if let Some(slot) = results.get_mut(index) {
                        *slot = Some(result);
                    }
                    drop(results);
                    index = next.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });

    let results = results.into_inner().unwrap_or_else(PoisonError::into_inner);
    results.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_order() {
        let items: Vec<u32> = (0..100).collect();
        assert_eq!(
            super::map(&items, |i| i * 2),
            items.iter().map(|i| i * 2).collect::<Vec<_>>()
        );
        assert!(super::map(&[] as &[u32], |i| *i).is_empty());
    }
}
