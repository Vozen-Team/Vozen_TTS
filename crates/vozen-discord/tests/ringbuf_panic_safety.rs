//! Regression for RUSTSEC-2026-0293 in the dependency used by Songbird.
use ringbuf::{HeapRb, traits::*};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

struct Item {
    drops: Arc<AtomicUsize>,
    armed: Arc<AtomicBool>,
}

impl Drop for Item {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        assert!(
            !self.armed.swap(false, Ordering::SeqCst),
            "synthetic drop panic"
        );
    }
}

#[test]
fn skipping_or_clearing_a_panicking_item_does_not_drop_it_twice() {
    for clear in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let mut buffer = HeapRb::new(4);
        for _ in 0..4 {
            assert!(
                buffer
                    .try_push(Item {
                        drops: drops.clone(),
                        armed: armed.clone()
                    })
                    .is_ok()
            );
        }
        armed.store(true, Ordering::SeqCst);
        assert!(
            catch_unwind(AssertUnwindSafe(|| {
                if clear {
                    buffer.clear();
                } else {
                    buffer.skip(2);
                }
            }))
            .is_err()
        );
        drop(buffer);
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }
}
