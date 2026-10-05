//! Completion is signalled after worker-owned connections/adapters have dropped.
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use tokio::sync::watch;

pub(crate) struct WorkersLifetime {
    live: AtomicUsize,
    closed: AtomicBool,
    done: watch::Sender<bool>,
}

impl WorkersLifetime {
    pub fn new() -> Arc<Self> {
        let (done, _) = watch::channel(false);
        Arc::new(Self {
            live: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            done,
        })
    }
    pub fn guard(self: &Arc<Self>) -> WorkerGuard {
        self.live.fetch_add(1, Ordering::SeqCst);
        WorkerGuard(Arc::clone(self))
    }
    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.check_done();
    }
    fn check_done(&self) {
        // These two atomics need a shared order: close and the last guard must
        // not both miss each other's update and lose the completion notification.
        if self.closed.load(Ordering::SeqCst) && self.live.load(Ordering::SeqCst) == 0 {
            self.done.send_replace(true);
        }
    }
    pub fn is_done(&self) -> bool {
        *self.done.borrow()
    }
    pub async fn wait(&self) {
        let mut receiver = self.done.subscribe();
        while !*receiver.borrow_and_update() {
            if receiver.changed().await.is_err() {
                break;
            }
        }
    }
}

pub(crate) struct WorkerGuard(Arc<WorkersLifetime>);
impl Drop for WorkerGuard {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
        self.0.check_done();
    }
}
