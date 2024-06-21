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
            hint::spin_loop();
        }
        // Decrement ptr_counter_to_clear
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
    update_token: RwLock<()>,
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
        let ptr = unsafe {
            NonNull::new_unchecked(
                (self.ptr_counter_latest.fetch_add(1, Ordering::AcqRel) >> 64) as usize as *mut T,
            )
        };
        RcuGuard { cell: self, ptr }
    }

    /// Writes a new value into the `RcuCell`.
    ///
    /// This function immediately writes the new value into the `RcuCell`.
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
    /// This function applies the given closure `f` to the current value
    /// stored in the `RcuCell`, replacing it with the new value returned
    /// by the closure. It will block until all current readers have finished
    /// reading the old value.
    ///
    /// Once all readers have completed their read operations, the old value
    /// will be safely released.
    ///
    /// # Arguments
    ///
    /// * `f` - A closure that takes a reference to the current value and returns
    ///         a new value to store in the `RcuCell`.
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
    pub fn update(&self, mut f: impl FnMut(&T) -> T) {
        let token_exclusive = self.update_token.write();
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

    /// Clears the old value from memory once it is no longer needed.
    ///
    /// This function is called internally to release the memory of the old
    /// value after it has been replaced by a new value. It ensures that all
    /// readers have completed their read operations on the old value before
    /// freeing the memory.
    ///
    /// # Arguments
    ///
    /// * `old_ptr_counter` - The old pointer and counter value to be cleared.
    ///
    /// This function does not need to be called directly by users of the
    /// `RcuCell`.
    fn clear(&self, old_ptr_counter: u128) {
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
            .compare_exchange_weak(0, old_ptr_counter, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            // Inner loop to only get shared memory access (MESI protocal)
            while self.ptr_counter_to_clear.load(Ordering::Relaxed) != 0 {
                hint::spin_loop();
            }
        }

        // No need to use CAS here because when the counter is 0,
        // it will not be updated by other threads
        //
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
