#![cfg(target_has_atomic = "128")]
#![feature(integer_atomics)]
#![no_std]
extern crate alloc;
use alloc::boxed::Box;

use core::{
    hint,
    marker::PhantomData,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicU128, Ordering},
};

/// A guard that provides read access to a value in an `RcuCell`.
///
/// When this guard is dropped, it will signal that the read operation
/// is complete, allowing the `RcuCell` to manage its internal state
/// accordingly.
pub struct RcuGuard<'a, T> {
    ptr: NonNull<T>,
    cell: &'a RcuCell<T>,
}

impl<T> Deref for RcuGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> Drop for RcuGuard<'_, T> {
    fn drop(&mut self) {
        // Try to decrement ptr_counter_latest first
        loop {
            let ptr_counter_latest = self.cell.ptr_counter_latest.load(Ordering::Acquire);
            if (ptr_counter_latest >> 64) as usize == self.ptr.as_ptr() as usize {
                if self
                    .cell
                    .ptr_counter_latest
                    .compare_exchange_weak(
                        ptr_counter_latest,
                        ptr_counter_latest - 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    return;
                }
            } else {
                // ptr_counter_latest has been updated, so we can't decrement it
                break;
            }
            hint::spin_loop();
        }
        // Decrement ptr_counter_to_clear
        loop {
            let ptr_counter_old = self.cell.ptr_counter_to_clear.load(Ordering::Acquire);
            if (ptr_counter_old >> 64) as usize == self.ptr.as_ptr() as usize
                && self
                    .cell
                    .ptr_counter_to_clear
                    .compare_exchange_weak(
                        ptr_counter_old,
                        ptr_counter_old - 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
            {
                return;
            }
            hint::spin_loop();
        }
    }
}

/// A concurrent data structure that allows for safe, read-copy-update (RCU)
/// style access to its value.
pub struct RcuCell<T> {
    ptr_counter_latest: AtomicU128,
    ptr_counter_to_clear: AtomicU128,
    data: PhantomData<T>,
}

impl<T> RcuCell<T> {
    /// Creates a new `RcuCell` with the given initial value.
    ///
    /// This function initializes a new `RcuCell` instance, setting its
    /// initial value to the provided `value`.
    ///
    /// # Arguments
    ///
    /// * `value` - The initial value to store in the `RcuCell`.
    ///
    /// # Returns
    ///
    /// A new instance of `RcuCell` containing the provided initial value.
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
        }
    }

    /// Provides read access to the value stored in the `RcuCell`.
    ///
    /// This function returns an `RcuGuard`, which allows for safe,
    /// concurrent read access to the `RcuCell`'s value.
    ///
    /// Once all `RcuGuard` instances referencing a particular value are
    /// dropped, the value can be safely released during an update.
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
        let ptr = unsafe {
            NonNull::new_unchecked(
                (self.ptr_counter_latest.fetch_add(1, Ordering::AcqRel) >> 64) as usize as *mut T,
            )
        };
        RcuGuard { cell: self, ptr }
    }

    /// Updates to a new value.
    ///
    /// This function immediately updates the value in the `RcuCell`.
    /// It will block until all current readers have finished reading
    /// the old value.
    ///
    /// Once all readers have completed their read operations, the
    /// old value will be safely released.
    ///
    /// # Arguments
    ///
    /// * `value` - The new value to store in the `RcuCell`.
    ///
    /// # Example
    ///
    /// ```
    /// let rcu_cell = rcu_128::RcuCell::new(42);
    /// rcu_cell.update(100);
    /// {
    ///     let guard = rcu_cell.read();
    ///     assert_eq!(*guard, 100);
    /// }
    /// ```
    pub fn update(&self, value: T) {
        let new_ptr_counter = (Box::into_raw(Box::new(value)) as u128) << 64;
        let old_ptr_counter = self
            .ptr_counter_latest
            .swap(new_ptr_counter, Ordering::AcqRel);
        if old_ptr_counter & 0xffff_ffff_ffff_ffff == 0 {
            // No reader, release memory directly
            unsafe {
                let _ = Box::from_raw((old_ptr_counter >> 64) as usize as *mut T);
            }
            return;
        }
        // Only one thread can clear ptr_counter_to_clear at the same time
        while self
            .ptr_counter_to_clear
            .compare_exchange_weak(0, old_ptr_counter, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            hint::spin_loop();
        }
        // Wait for all readers to finish
        while self.ptr_counter_to_clear.load(Ordering::Acquire) & 0xffff_ffff_ffff_ffff != 0 {
            hint::spin_loop();
        }
        // Clear ptr_counter_to_clear to allow other writers to release memory
        self.ptr_counter_to_clear.store(0, Ordering::Release);
        unsafe {
            let _ = Box::from_raw((old_ptr_counter >> 64) as usize as *mut T);
        }
    }
}
