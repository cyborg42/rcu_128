#![feature(allocator_api)]
#![feature(integer_atomics)]
use std::{
    alloc::{Allocator, Global, Layout},
    marker::PhantomData,
    ops::Deref,
    ptr::NonNull,
    sync::atomic::{AtomicU128, Ordering},
};

use parking_lot::Mutex;

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
        // try to decrement ptr_counter_latest first
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
                // cell.ptr_counter_latest has been updated, so we can't decrement ptr_counter_latest
                break;
            }
            std::hint::spin_loop();
        }
        // decrement ptr_counter_to_clear
        loop {
            let ptr_counter_old = self.cell.ptr_counter_to_clear.load(Ordering::Acquire);
            if (ptr_counter_old >> 64) as usize == self.ptr.as_ptr() as usize {
                if self
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
            }
            std::hint::spin_loop();
        }
    }
}

pub struct RcuCell<T> {
    ptr_counter_latest: AtomicU128,
    ptr_counter_to_clear: AtomicU128,
    write_token: Mutex<()>,
    data: PhantomData<T>,
}

impl<T> RcuCell<T> {
    pub fn new(value: T) -> Self {
        Self {
            ptr_counter_latest: AtomicU128::new((Box::into_raw(Box::new(value)) as u128) << 64),
            ptr_counter_to_clear: AtomicU128::new(0),
            write_token: Mutex::new(()),
            data: PhantomData,
        }
    }
    pub fn read(&self) -> RcuGuard<'_, T> {
        let ptr = unsafe {
            NonNull::new_unchecked(
                (self.ptr_counter_latest.fetch_add(1, Ordering::AcqRel) >> 64) as usize as *mut T,
            )
        };
        RcuGuard { cell: &self, ptr }
    }
    pub fn write(&self, value: T) {
        let new_ptr_counter = (Box::into_raw(Box::new(value)) as u128) << 64;
        let old_ptr_counter = self
            .ptr_counter_latest
            .swap(new_ptr_counter, Ordering::AcqRel);
        if old_ptr_counter & 0xffff_ffff_ffff_ffff == 0 {
            // no reader, release memory directly
            unsafe {
                let ptr = NonNull::new_unchecked((old_ptr_counter >> 64) as usize as *mut T);
                std::ptr::drop_in_place(ptr.as_ptr());
                Global.deallocate(ptr.cast(), Layout::new::<T>());
            }
            return;
        }
        // only one thread can clear ptr_counter_to_clear at the same time
        let write_guard = self.write_token.lock();
        self.ptr_counter_to_clear
            .store(old_ptr_counter, Ordering::Release);
        // wait for all readers to finish
        while self.ptr_counter_to_clear.load(Ordering::Acquire) & 0xffff_ffff_ffff_ffff != 0 {
            std::hint::spin_loop();
        }
        // clear ptr_counter_to_clear to prevent being same with new_ptr_counter
        self.ptr_counter_to_clear.store(0, Ordering::Release);
        drop(write_guard);
        unsafe {
            let ptr = NonNull::new_unchecked((old_ptr_counter >> 64) as usize as *mut T);
            std::ptr::drop_in_place(ptr.as_ptr());
            Global.deallocate(ptr.cast(), Layout::new::<T>());
        }
    }
}

#[test]
fn test() {
    use std::thread::sleep;
    let x = RcuCell::new("0".to_string());
    let _t = x.read().clone();
    std::thread::scope(|s| {
        s.spawn(|| {
            let mut i = 0;
            loop {
                sleep(std::time::Duration::from_millis(100));
                i += 1;
                let t = std::time::Instant::now();
                x.write(i.to_string());
                println!("{:?}", t.elapsed());
            }
        });
        s.spawn(|| {
            let mut idx: usize = 0;
            let mut guards: [RcuGuard<String>; 4] = [x.read(), x.read(), x.read(), x.read()];
            loop {
                let r = x.read();
                println!("{}", *r);
                guards[idx % 4] = r;
                idx += 1;
                sleep(std::time::Duration::from_millis(10));
            }
        });
    })
}
