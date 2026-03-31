#![cfg(target_has_atomic = "128")]
#![feature(integer_atomics)]
#![no_std]
extern crate alloc;
use alloc::boxed::Box;
use parking_lot::RwLock;

use core::{
    hint,
    marker::PhantomData,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicU128, Ordering},
};

const COUNTER_MASK: u128 = 0xffff_ffff_ffff_ffff;

/// Exponential backoff for spin-wait loops.
///
/// Starts with `spin_loop` hints, then yields the thread after a threshold
/// to avoid burning CPU when waiting for long-lived readers.
struct Backoff {
    step: u32,
}

impl Backoff {
    fn new() -> Self {
        Self { step: 0 }
    }

    fn spin(&mut self) {
        for _ in 0..1u32 << self.step.min(6) {
            hint::spin_loop();
        }
        if self.step <= 6 {
            self.step += 1;
        }
    }
}

/// A guard that provides read access to a value in an `RcuCell`.
///
/// When this guard is dropped, it will signal that the read operation
/// is complete, allowing the `RcuCell` to manage its internal state
/// accordingly.
#[derive(Debug)]
pub struct RcuGuard<'a, T> {
    ptr: NonNull<T>,
    cell: &'a RcuCell<T>,
}

impl<T> Deref for RcuGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The pointer was obtained from a valid Box allocation in RcuCell::new,
        // write, or update. The value is kept alive because:
        // - If the value is still current (in ptr_counter_latest), it won't be freed
        //   until swapped out and all readers drain.
        // - If swapped out (in ptr_counter_to_clear), the writer spins until our
        //   guard's counter decrement, so the value is alive while this guard exists.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> Drop for RcuGuard<'_, T> {
    fn drop(&mut self) {
        // Try to decrement ptr_counter_latest first (fast path: value hasn't been swapped)
        let mut backoff = Backoff::new();
        loop {
            let ptr_counter = self.cell.ptr_counter_latest.load(Ordering::Acquire);
            if (ptr_counter >> 64) as usize == self.ptr.as_ptr() as usize {
                if self
                    .cell
                    .ptr_counter_latest
                    .compare_exchange_weak(
                        ptr_counter,
                        ptr_counter - 1,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return;
                }
            } else {
                // ptr_counter_latest has been updated, so we can't decrement it
                break;
            }
            backoff.spin();
        }
        // Slow path: value was swapped out, decrement ptr_counter_to_clear.
        // The writer that swapped our value out will (or has already) moved it
        // into ptr_counter_to_clear. We spin until it appears there.
        let mut backoff = Backoff::new();
        loop {
            let ptr_counter = self.cell.ptr_counter_to_clear.load(Ordering::Acquire);
            if (ptr_counter >> 64) as usize == self.ptr.as_ptr() as usize
                && self
                    .cell
                    .ptr_counter_to_clear
                    .compare_exchange_weak(
                        ptr_counter,
                        ptr_counter - 1,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
            {
                return;
            }
            backoff.spin();
        }
    }
}

/// A concurrent data structure that allows for safe, read-copy-update (RCU)
/// style access to its value.
///
/// # Grace period serialization
///
/// Only one old value can be pending reclamation at a time (single
/// `ptr_counter_to_clear` slot). If multiple writers call [`write`](RcuCell::write)
/// concurrently while readers hold guards to old values, their grace periods
/// are serialized. This is acceptable for read-heavy workloads but can cause
/// writer stalls under heavy write contention with long-lived readers.
#[derive(Debug)]
pub struct RcuCell<T> {
    ptr_counter_latest: AtomicU128,
    ptr_counter_to_clear: AtomicU128,
    data: PhantomData<T>,
    update_token: RwLock<()>,
}

impl<T: Default> Default for RcuCell<T> {
    fn default() -> Self {
        Self::new(Default::default())
    }
}

impl<T> Drop for RcuCell<T> {
    fn drop(&mut self) {
        // SAFETY: All RcuGuards borrow &RcuCell, so the borrow checker guarantees
        // they are all dropped before this runs. Therefore the counter is 0 and we
        // have exclusive ownership of the value. get_mut() is sound because &mut self.
        let ptr = (*self.ptr_counter_latest.get_mut() >> 64) as usize as *mut T;
        unsafe {
            let _ = Box::from_raw(ptr);
        }
    }
}

impl<T> RcuCell<T> {
    /// Creates a new `RcuCell` with the given initial value.
    ///
    /// # Example
    ///
    /// ```
    /// let rcu_cell = rcu_128::RcuCell::new(42);
    /// ```
    pub fn new(value: T) -> Self {
        Self {
            ptr_counter_latest: AtomicU128::new((Box::into_raw(Box::new(value)) as u128) << 64),
            ptr_counter_to_clear: AtomicU128::new(0),
            data: PhantomData,
            update_token: RwLock::new(()),
        }
    }

    /// Provides read access to the value stored in the `RcuCell`.
    ///
    /// This function returns an `RcuGuard`, which allows for safe,
    /// concurrent read access to the `RcuCell`'s value.
    ///
    /// Once all `RcuGuard` instances referencing a particular value are
    /// dropped, the value can be safely released during an update or write.
    ///
    /// # Example
    ///
    /// ```
    /// let rcu_cell = rcu_128::RcuCell::new(42);
    /// {
    ///     let guard = rcu_cell.read();
    ///     assert_eq!(*guard, 42);
    /// }
    /// ```
    pub fn read(&self) -> RcuGuard<'_, T> {
        // SAFETY: The upper 64 bits of ptr_counter_latest always hold a valid,
        // non-null pointer to a Box-allocated T. fetch_add atomically increments
        // the reader count, which prevents the writer from freeing this value
        // until we drop the guard.
        let ptr = unsafe {
            NonNull::new_unchecked(
                (self.ptr_counter_latest.fetch_add(1, Ordering::AcqRel) >> 64) as usize as *mut T,
            )
        };
        RcuGuard { cell: self, ptr }
    }

    /// Writes a new value into the `RcuCell`.
    ///
    /// The new value becomes immediately visible to subsequent readers.
    /// This method blocks until all readers of the old value have dropped
    /// their guards, then frees the old value.
    ///
    /// Multiple concurrent `write` calls are allowed (last-writer-wins).
    /// Use [`update`](RcuCell::update) if you need read-modify-write semantics.
    ///
    /// # Example
    ///
    /// ```
    /// let rcu_cell = rcu_128::RcuCell::new(42);
    /// rcu_cell.write(100);
    /// {
    ///     let guard = rcu_cell.read();
    ///     assert_eq!(*guard, 100);
    /// }
    /// ```
    pub fn write(&self, value: T) {
        let new_ptr_counter = (Box::into_raw(Box::new(value)) as u128) << 64;
        let token_shared = self.update_token.read();
        let old_ptr_counter = self
            .ptr_counter_latest
            .swap(new_ptr_counter, Ordering::AcqRel);
        drop(token_shared);
        self.clear(old_ptr_counter);
    }

    /// Updates the value stored in the `RcuCell` using a provided function.
    ///
    /// This function applies the given closure `f` to the current value,
    /// replacing it with the returned value. The closure runs under an
    /// exclusive lock, so concurrent `update` and `write` calls are
    /// serialized — the closure is guaranteed to run exactly once.
    ///
    /// # Example
    ///
    /// ```
    /// let rcu_cell = rcu_128::RcuCell::new(42);
    /// rcu_cell.update(|&old_value| old_value + 1);
    /// {
    ///     let guard = rcu_cell.read();
    ///     assert_eq!(*guard, 43);
    /// }
    /// ```
    pub fn update(&self, f: impl FnOnce(&T) -> T) {
        let token_exclusive = self.update_token.write();
        // SAFETY: The exclusive lock prevents any concurrent write/update from
        // swapping out ptr_counter_latest's pointer. Readers only obtain shared
        // references (&T) via guards, so no mutable aliasing occurs. The pointer
        // is valid because it was produced by Box::into_raw and hasn't been freed
        // (clear() only frees values after they're swapped out of ptr_counter_latest).
        let old_value =
            unsafe { &*((self.ptr_counter_latest.load(Ordering::Acquire) >> 64) as *const T) };
        let new_value = f(old_value);
        let new_ptr_counter = (Box::into_raw(Box::new(new_value)) as u128) << 64;
        let old_ptr_counter = self
            .ptr_counter_latest
            .swap(new_ptr_counter, Ordering::AcqRel);
        drop(token_exclusive);
        self.clear(old_ptr_counter);
    }

    /// Waits for all readers of the old value to finish, then frees it.
    fn clear(&self, old_ptr_counter: u128) {
        if old_ptr_counter & COUNTER_MASK == 0 {
            // No readers — release memory directly.
            // SAFETY: The pointer was produced by Box::into_raw. The counter is 0,
            // meaning no guards hold this pointer (the swap was atomic with the
            // reader's fetch_add, so any reader that incremented the count is
            // reflected here). It is safe to reclaim.
            unsafe {
                let _ = Box::from_raw((old_ptr_counter >> 64) as usize as *mut T);
            }
            return;
        }

        // Acquire the single reclamation slot. Only one old value can be
        // pending in ptr_counter_to_clear at a time. Other writers spin here
        // until the slot is available (grace period serialization).
        let mut backoff = Backoff::new();
        while self
            .ptr_counter_to_clear
            .compare_exchange_weak(0, old_ptr_counter, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // Inner loop: read-only spin to avoid exclusive cache line access (MESI)
            while self.ptr_counter_to_clear.load(Ordering::Relaxed) != 0 {
                backoff.spin();
            }
        }

        // Wait for all readers of the old value to drop their guards.
        // Each guard drop decrements the counter in ptr_counter_to_clear.
        // No CAS needed here: once counter reaches 0, no other thread will
        // modify it (new readers get the latest pointer, not this one).
        let mut backoff = Backoff::new();
        while self.ptr_counter_to_clear.load(Ordering::Acquire) & COUNTER_MASK != 0 {
            backoff.spin();
        }
        // Clear the slot to allow other writers to reclaim their old values.
        self.ptr_counter_to_clear.store(0, Ordering::Release);
        // SAFETY: All readers have drained (counter == 0). The pointer was
        // produced by Box::into_raw and has not been freed elsewhere.
        unsafe {
            let _ = Box::from_raw((old_ptr_counter >> 64) as usize as *mut T);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    extern crate alloc;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use std::thread;

    #[test]
    fn basic_read_write() {
        let cell = RcuCell::new(42);
        assert_eq!(*cell.read(), 42);

        cell.write(100);
        assert_eq!(*cell.read(), 100);
    }

    #[test]
    fn update_applies_closure() {
        let cell = RcuCell::new(10);
        cell.update(|&v| v + 5);
        assert_eq!(*cell.read(), 15);

        cell.update(|&v| v * 2);
        assert_eq!(*cell.read(), 30);
    }

    #[test]
    fn default_trait() {
        let cell: RcuCell<i32> = RcuCell::default();
        assert_eq!(*cell.read(), 0);
    }

    #[test]
    fn multiple_guards_same_value() {
        let cell = RcuCell::new(42);
        let g1 = cell.read();
        let g2 = cell.read();
        let g3 = cell.read();
        assert_eq!(*g1, 42);
        assert_eq!(*g2, 42);
        assert_eq!(*g3, 42);
        drop(g1);
        drop(g2);
        drop(g3);
    }

    #[test]
    fn guard_sees_value_at_read_time() {
        let cell = Arc::new(RcuCell::new(1));
        let guard = cell.read();

        // Write from another thread (write blocks until guard drops)
        let cell2 = cell.clone();
        let handle = thread::spawn(move || {
            cell2.write(2);
        });

        // Guard still sees the old value
        assert_eq!(*guard, 1);
        drop(guard);
        handle.join().unwrap();
        assert_eq!(*cell.read(), 2);
    }

    #[test]
    fn drop_frees_value() {
        // Use Arc to verify the value is freed when RcuCell is dropped.
        let inner = Arc::new(42);
        let cell = RcuCell::new(inner.clone());
        assert_eq!(Arc::strong_count(&inner), 2);
        drop(cell);
        assert_eq!(Arc::strong_count(&inner), 1);
    }

    #[test]
    fn write_frees_old_value() {
        let v1 = Arc::new(1);
        let v2 = Arc::new(2);
        let cell = RcuCell::new(v1.clone());
        assert_eq!(Arc::strong_count(&v1), 2);

        cell.write(v2.clone());
        // old value should be freed since no guards held it
        assert_eq!(Arc::strong_count(&v1), 1);
        assert_eq!(Arc::strong_count(&v2), 2);
    }

    #[test]
    #[cfg(not(miri))] // requires threads + spin-wait, too slow for Miri
    fn old_value_freed_after_guard_drop() {
        let v1 = Arc::new(1);
        let cell = RcuCell::new(v1.clone());
        let guard = cell.read();
        assert_eq!(Arc::strong_count(&v1), 2);

        // Spawn a thread to write, which will block until we drop the guard
        let cell_ref = &cell;
        let v2 = Arc::new(2);
        let v2_clone = v2.clone();
        thread::scope(|s| {
            s.spawn(move || {
                cell_ref.write(v2_clone);
            });
            // Give the writer time to swap (but it will spin on clear)
            thread::sleep(std::time::Duration::from_millis(10));
            // v1 still alive because guard holds it
            assert_eq!(Arc::strong_count(&v1), 2);
            drop(guard);
        });
        // After scope, writer thread joined, v1 should be freed
        assert_eq!(Arc::strong_count(&v1), 1);
        assert_eq!(*cell.read(), v2);
    }

    #[test]
    #[cfg(not(miri))]
    fn concurrent_readers() {
        let cell = Arc::new(RcuCell::new(0u64));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let cell = cell.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    let guard = cell.read();
                    let _ = *guard; // just read
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }

    #[test]
    #[cfg(not(miri))]
    fn concurrent_read_write() {
        let cell = Arc::new(RcuCell::new(0u64));

        thread::scope(|s| {
            // Writer
            let cell_w = cell.clone();
            s.spawn(move || {
                for i in 0..100 {
                    cell_w.write(i);
                }
            });

            // Readers
            for _ in 0..4 {
                let cell_r = cell.clone();
                s.spawn(move || {
                    for _ in 0..1000 {
                        let guard = cell_r.read();
                        let val = *guard;
                        assert!(val < 100);
                    }
                });
            }
        });
    }

    #[test]
    #[cfg(not(miri))]
    fn concurrent_updates() {
        let cell = Arc::new(RcuCell::new(0u64));

        thread::scope(|s| {
            for _ in 0..4 {
                let cell = cell.clone();
                s.spawn(move || {
                    for _ in 0..100 {
                        cell.update(|&v| v + 1);
                    }
                });
            }
        });

        assert_eq!(*cell.read(), 400);
    }

    #[test]
    #[cfg(not(miri))]
    fn stress_mixed_operations() {
        let cell = Arc::new(RcuCell::new(0u64));

        thread::scope(|s| {
            // Writers
            for _ in 0..2 {
                let cell = cell.clone();
                s.spawn(move || {
                    for i in 0..20 {
                        cell.write(i);
                    }
                });
            }

            // Updaters
            for _ in 0..2 {
                let cell = cell.clone();
                s.spawn(move || {
                    for _ in 0..20 {
                        cell.update(|&v| v.wrapping_add(1));
                    }
                });
            }

            // Readers with held guards
            for _ in 0..2 {
                let cell = cell.clone();
                s.spawn(move || {
                    let mut guards = Vec::new();
                    for i in 0..80 {
                        guards.push(cell.read());
                        if guards.len() > 4 {
                            guards.remove(0);
                        }
                        if i % 10 == 0 {
                            guards.clear();
                        }
                    }
                });
            }
        });
    }
}
