//! Models the notification generation captured by `Notify::notified` and the
//! completion load. This does not model every Tokio scheduler implementation.
use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::thread;

fn completion_model(register_first: bool) {
    loom::model(move || {
        let complete = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicUsize::new(0));
        let done = complete.clone();
        let notified = generation.clone();
        let publisher = thread::spawn(move || {
            done.store(true, Ordering::Release);
            notified.fetch_add(1, Ordering::Release);
        });
        let (registered, ready) = if register_first {
            (
                generation.load(Ordering::Acquire),
                complete.load(Ordering::Acquire),
            )
        } else {
            let ready = complete.load(Ordering::Acquire);
            (generation.load(Ordering::Acquire), ready)
        };
        publisher.join().unwrap();
        // A waiter which saw false must have captured an older notification.
        assert!(
            ready || generation.load(Ordering::Acquire) != registered,
            "completion notification was lost between check and registration"
        );
    });
}

#[test]
fn register_before_check_never_loses_completion() {
    completion_model(true);
}

#[test]
#[should_panic(expected = "completion notification was lost")]
fn model_detects_the_original_check_before_register_bug() {
    completion_model(false);
}
