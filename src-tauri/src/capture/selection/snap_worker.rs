//! 每个选区会话一个吸附线程:独占 `FrameSnap`,合并最新光标请求。
//!
//! UI 线程只 `submit` / `poll`,不等待、不 join。平台 `hit` 卡住时 `Drop`
//! 分离线程,最多留到进程退出,不阻塞下一次会话。

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};

use crate::capture::snap::{FrameSnap, SnapRequest, SnapResult};

struct Slot {
    request: Option<SnapRequest>,
    stop: bool,
}

struct Mailbox {
    slot: Mutex<Slot>,
    slot_cv: Condvar,
    result: Mutex<Option<SnapResult>>,
    result_cv: Condvar,
    /// 结果已经能被 `poll` 取到之后,在 worker 线程调用。不得阻塞。
    wake: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// 会话级吸附 worker。结果带请求的代际与光标。
pub struct SnapWorker {
    mailbox: Arc<Mailbox>,
    thread: Option<JoinHandle<()>>,
}

impl SnapWorker {
    pub fn spawn(snap: FrameSnap) -> Self {
        Self::spawn_with_wake(snap, None)
    }

    /// `wake` 在结果写入且 `poll` 可见之后于 worker 线程调用。
    /// 回调不得阻塞,也不得等待 UI 线程处理完这次唤醒。
    pub fn spawn_with_wake(snap: FrameSnap, wake: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        let mailbox = Arc::new(Mailbox {
            slot: Mutex::new(Slot {
                request: None,
                stop: false,
            }),
            slot_cv: Condvar::new(),
            result: Mutex::new(None),
            result_cv: Condvar::new(),
            wake,
        });
        let shared = Arc::clone(&mailbox);
        let thread = thread::Builder::new()
            .name("cropmark-snap".to_string())
            .spawn(move || worker_loop(snap, shared))
            .ok();
        Self { mailbox, thread }
    }

    /// 提交查询。已有更旧的未处理请求会被盖掉;更旧的代际不会盖住更新的。
    pub fn submit(&self, request: SnapRequest) {
        {
            let mut slot = lock(&self.mailbox.slot);
            if slot.stop {
                return;
            }
            if !store_latest(&mut slot.request, request) {
                return;
            }
        }
        self.mailbox.slot_cv.notify_one();
    }

    /// 取走一份已完成的结果。没有则立即返回,不阻塞 UI。
    pub fn poll(&self) -> Option<SnapResult> {
        lock(&self.mailbox.result).take()
    }

    #[cfg(test)]
    pub(crate) fn poll_timeout(&self, timeout: Duration) -> Option<SnapResult> {
        use std::time::Instant;

        let mut result = lock(&self.mailbox.result);
        let started = Instant::now();
        loop {
            if let Some(result) = result.take() {
                return Some(result);
            }
            let Some(remaining) = timeout.checked_sub(started.elapsed()) else {
                return None;
            };
            let (guard, status) = self
                .mailbox
                .result_cv
                .wait_timeout(result, remaining)
                .unwrap_or_else(|err| err.into_inner());
            result = guard;
            if status.timed_out() {
                return result.take();
            }
        }
    }
}

impl Drop for SnapWorker {
    fn drop(&mut self) {
        {
            let mut slot = lock(&self.mailbox.slot);
            slot.stop = true;
        }
        self.mailbox.slot_cv.notify_one();
        // `JoinHandle` 的 Drop 只分离,不 join。卡在平台 hit 时不能拖住会话退出。
        drop(self.thread.take());
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|err| err.into_inner())
}

/// 保留代际更大(或相等)的请求。返回是否写入。
fn store_latest(slot: &mut Option<SnapRequest>, request: SnapRequest) -> bool {
    let replace = slot
        .map(|current| request.generation >= current.generation)
        .unwrap_or(true);
    if replace {
        *slot = Some(request);
    }
    replace
}

fn worker_loop(snap: FrameSnap, mailbox: Arc<Mailbox>) {
    loop {
        let Some(request) = wait_request(&mailbox) else {
            return;
        };
        let result = snap.query(request);
        if lock(&mailbox.slot).request.is_some() {
            continue;
        }
        publish(&mailbox, result);
    }
}

fn wait_request(mailbox: &Mailbox) -> Option<SnapRequest> {
    let mut slot = lock(&mailbox.slot);
    loop {
        if let Some(request) = slot.request.take() {
            return Some(request);
        }
        if slot.stop {
            return None;
        }
        slot = mailbox
            .slot_cv
            .wait(slot)
            .unwrap_or_else(|err| err.into_inner());
    }
}

fn publish(mailbox: &Mailbox, result: SnapResult) {
    let stored = {
        let mut slot = lock(&mailbox.result);
        let replace = slot
            .as_ref()
            .map(|current| result.generation >= current.generation)
            .unwrap_or(true);
        if replace {
            *slot = Some(result);
        }
        replace
    };
    mailbox.result_cv.notify_one();
    if stored {
        if let Some(wake) = &mailbox.wake {
            wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::capture::geometry::PhysicalRect;
    use crate::capture::selection::{FeatureFlags, InputEvent, SelectionEngine};
    use crate::capture::snap::{
        FrameSnap, SnapCapability, SnapContext, SnapHit, SnapKind, SnapProvider, SnapRect,
        SnapStack,
    };

    fn request(generation: u64) -> SnapRequest {
        SnapRequest {
            generation,
            cursor: (generation as i32, 0),
        }
    }

    #[test]
    fn pending_slot_keeps_only_the_newest_generation() {
        let mut slot = None;
        assert!(store_latest(&mut slot, request(1)));
        assert!(store_latest(&mut slot, request(3)));
        assert!(store_latest(&mut slot, request(3)));
        assert!(!store_latest(&mut slot, request(2)));
        assert_eq!(slot.unwrap().generation, 3);
    }

    #[derive(Debug)]
    struct HitGate {
        hits: Mutex<Vec<i32>>,
        hit_cv: Condvar,
        permits: Mutex<usize>,
        permit_cv: Condvar,
    }

    impl HitGate {
        fn new() -> Self {
            Self {
                hits: Mutex::new(Vec::new()),
                hit_cv: Condvar::new(),
                permits: Mutex::new(0),
                permit_cv: Condvar::new(),
            }
        }

        fn record(&self, x: i32) {
            lock(&self.hits).push(x);
            self.hit_cv.notify_all();
        }

        fn wait_hits(&self, count: usize) {
            let mut hits = lock(&self.hits);
            let started = std::time::Instant::now();
            while hits.len() < count {
                let remaining = Duration::from_secs(2)
                    .checked_sub(started.elapsed())
                    .expect("timed out waiting for snap hits");
                let (guard, status) = self
                    .hit_cv
                    .wait_timeout(hits, remaining)
                    .unwrap_or_else(|err| err.into_inner());
                hits = guard;
                if status.timed_out() {
                    panic!("timed out waiting for {count} hits, saw {}", hits.len());
                }
            }
        }

        fn wait_permit(&self) {
            let mut permits = lock(&self.permits);
            while *permits == 0 {
                permits = self
                    .permit_cv
                    .wait(permits)
                    .unwrap_or_else(|err| err.into_inner());
            }
            *permits -= 1;
        }

        fn release(&self) {
            *lock(&self.permits) += 1;
            self.permit_cv.notify_one();
        }

        fn seen(&self) -> Vec<i32> {
            lock(&self.hits).clone()
        }
    }

    #[derive(Debug)]
    struct BlockingProvider {
        gate: &'static HitGate,
    }

    impl SnapProvider for BlockingProvider {
        fn capability(&self) -> SnapCapability {
            SnapCapability::Full
        }

        fn hit(&self, _context: SnapContext, x: i32, _y: i32) -> SnapStack {
            self.gate.record(x);
            self.gate.wait_permit();
            SnapStack::empty()
        }
    }

    #[test]
    fn worker_coalesces_queued_requests_and_drops_the_in_flight_result() {
        let gate: &'static HitGate = Box::leak(Box::new(HitGate::new()));
        let provider: &'static BlockingProvider = Box::leak(Box::new(BlockingProvider { gate }));
        let context = SnapContext::new((0, 0), (0, 0), 1.0);
        let worker = SnapWorker::spawn(FrameSnap::new(provider, context, (80, 60)));

        worker.submit(request(1));
        gate.wait_hits(1);
        worker.submit(request(5));
        worker.submit(request(4));
        worker.submit(request(5));
        gate.release();
        gate.wait_hits(2);
        assert_eq!(gate.seen(), vec![1, 5]);
        assert!(worker.poll().is_none());

        gate.release();
        let result = worker
            .poll_timeout(Duration::from_secs(2))
            .expect("latest snap result");
        assert_eq!(result.generation, 5);
        assert_eq!(result.cursor, (5, 0));
        assert!(result.stack.is_empty());
        assert!(worker.poll().is_none());
    }

    #[derive(Debug)]
    struct ThreadProvider {
        hits: &'static Mutex<Vec<thread::ThreadId>>,
    }

    impl SnapProvider for ThreadProvider {
        fn capability(&self) -> SnapCapability {
            SnapCapability::Full
        }

        fn hit(&self, _context: SnapContext, x: i32, y: i32) -> SnapStack {
            lock(&self.hits).push(thread::current().id());
            SnapStack::new(vec![SnapHit {
                kind: SnapKind::Control,
                rect: SnapRect::new(x, y, 8, 8),
                label: None,
            }])
        }
    }

    #[test]
    fn pointer_move_hits_on_the_worker_and_apply_uses_that_result() {
        let hits: &'static Mutex<Vec<thread::ThreadId>> =
            Box::leak(Box::new(Mutex::new(Vec::new())));
        let provider: &'static ThreadProvider = Box::leak(Box::new(ThreadProvider { hits }));
        let context = SnapContext::new((0, 0), (0, 0), 1.0);
        let mut engine =
            SelectionEngine::new(120, 80, FeatureFlags::default()).with_snap(provider, context);
        let worker = SnapWorker::spawn(FrameSnap::new(provider, context, engine.size()));

        engine.handle_event(InputEvent::PointerMove { x: 16, y: 18 });
        assert!(lock(hits).is_empty());
        assert!(engine.snap_highlight().is_none());
        let request = engine.take_snap_request().expect("queued request");
        assert_eq!(request.cursor, (16, 18));
        worker.submit(request);
        let result = worker
            .poll_timeout(Duration::from_secs(2))
            .expect("worker result");
        let seen = lock(hits).clone();
        assert_eq!(seen.len(), 1);
        assert_ne!(seen[0], thread::current().id());
        assert_eq!(result.generation, request.generation);
        assert_eq!(result.cursor, request.cursor);
        assert!(engine.apply_snap_result(result));
        assert_eq!(
            engine.snap_highlight(),
            Some(PhysicalRect {
                x: 16,
                y: 18,
                width: 8,
                height: 8,
            })
        );
    }

    #[test]
    fn wake_is_signaled_only_after_the_result_can_be_polled() {
        use std::sync::{mpsc, Arc};

        #[derive(Debug)]
        struct EmptyProvider;

        impl SnapProvider for EmptyProvider {
            fn capability(&self) -> SnapCapability {
                SnapCapability::Full
            }

            fn hit(&self, _context: SnapContext, _x: i32, _y: i32) -> SnapStack {
                SnapStack::empty()
            }
        }

        static EMPTY: EmptyProvider = EmptyProvider;
        let (tx, rx) = mpsc::channel();
        let worker = SnapWorker::spawn_with_wake(
            FrameSnap::new(&EMPTY, SnapContext::new((0, 0), (0, 0), 1.0), (80, 60)),
            Some(Arc::new(move || {
                let _ = tx.send(());
            })),
        );
        worker.submit(request(1));
        rx.recv_timeout(Duration::from_secs(2))
            .expect("wake after publish");
        let result = worker.poll().expect("result visible when wake runs");
        assert_eq!(result.generation, 1);
        assert_eq!(result.cursor, (1, 0));
        assert!(result.stack.is_empty());
    }
}
