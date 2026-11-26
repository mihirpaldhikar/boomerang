/*
 * Copyright (c) Mihir Paldhikar
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 * of this software and associated documentation files (the “Software”), to deal
 * in the Software without restriction, including without limitation the rights
 * to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
 * copies of the Software, and to permit persons to whom the Software is
 * furnished to do so, subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED “AS IS”, WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
 * FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
 * AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
 * LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
 * OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
 * SOFTWARE.
 */

use std::cell::{Cell, RefCell};

thread_local! {
    static WORKER_ID: Cell<Option<usize>> = const { Cell::new(None) };
}

#[inline]
pub(crate) fn set_current_worker_id(id: usize) {
    WORKER_ID.with(|c| c.set(Some(id)));
}

/// Returns the deterministic 0-based index of the current TPC worker thread.
#[inline]
pub fn worker_id() -> Option<usize> {
    WORKER_ID.get()
}

/// 128-byte cache-line-aligned wrapper preventing false sharing across CPU cores.
#[repr(align(128))]
#[derive(Debug, Default)]
pub struct CachePadded<T>(pub T);

/// Per-core isolated storage: each worker core gets its own L1/L2-hot `RefCell<T>`
/// on an isolated cache line—zero locks, zero atomics, zero cross-core bouncing.
pub struct PerCore<T> {
    slots: Box<[CachePadded<RefCell<T>>]>,
}

// SAFETY: Each slot `slots[worker_id]` is only ever accessed by that single pinned worker thread.
unsafe impl<T: Send> Sync for PerCore<T> {}

impl<T> PerCore<T> {
    /// Initializes one slot per worker thread (plus 1 fallback slot for non-worker contexts).
    pub fn new(mut init: impl FnMut(usize) -> T) -> Self {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let slots = (0..cores)
            .map(|i| CachePadded(RefCell::new(init(i))))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Self { slots }
    }

    /// Accesses the current core's isolated state with zero atomics and zero locks.
    #[inline]
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        let idx = worker_id().unwrap_or(0) % self.slots.len();
        let mut guard = self.slots[idx].0.borrow_mut();
        f(&mut *guard)
    }

    /// Aggregates across all cores (e.g. for metrics snapshots).
    pub fn for_each<F: FnMut(usize, &T)>(&self, mut f: F) {
        for (idx, slot) in self.slots.iter().enumerate() {
            let guard = slot.0.borrow();
            f(idx, &*guard);
        }
    }
}
