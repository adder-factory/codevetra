use shuttle::{
    Config, MaxSteps, Runner,
    scheduler::{PctScheduler, RandomScheduler, ReplayScheduler},
};

// The error adapter preserves the variants used by the bound production bodies.
// Formatting, native I/O, clocks and weak-memory behavior are separate test planes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RocksError {
    InvalidInput { field: &'static str },
    Limit { resource: &'static str },
    Operation { operation: &'static str },
    Transition,
    Cancelled,
    Closed,
}
type Result<T> = std::result::Result<T, RocksError>;
fn operation(operation: &'static str) -> RocksError {
    RocksError::Operation { operation }
}

mod instrumented_std {
    pub mod sync {
        pub use shuttle::sync::{Arc, Mutex};
    }
}

mod model_tokio {
    pub use tokio::{pin, select, spawn, sync};

    pub mod time {
        pub use tokio::time::Instant;

        // Shuttle's Sleep can complete immediately regardless of this deadline.
        // These scenarios isolate cancellation/notification races; the real
        // runtime suite owns deadline expiry. Keep the timer branch pending.
        pub async fn sleep_until(_deadline: Instant) {
            std::future::pending::<()>().await;
        }
    }
}

mod runtime {
    use super::{Result, RocksError};
    use crate::model_tokio as tokio;
    use shuttle::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::{
        future::{Future, poll_fn},
        task::Poll,
        time::{Duration, Instant},
    };
    use tokio::sync::{Notify, oneshot, watch};

    include!(concat!(env!("OUT_DIR"), "/request.rs"));

    mod group {
        use crate::instrumented_std as std;
        use crate::model_tokio as tokio;
        include!(concat!(env!("OUT_DIR"), "/group.rs"));
    }
    use group::StageGroup;

    fn request() -> StorageRequest {
        StorageRequest::new(Duration::from_secs(300)).unwrap()
    }

    pub fn cancellation_wakes_all() {
        shuttle::future::block_on(async {
            let cancellation = Cancellation::default();
            let count = Arc::new(AtomicUsize::new(0));
            let mut waiters = Vec::new();
            for _ in 0..4 {
                let cancellation = cancellation.clone();
                let count = Arc::clone(&count);
                waiters.push(tokio::spawn(async move {
                    cancellation.cancelled().await;
                    assert!(cancellation.is_cancelled());
                    count.fetch_add(1, Ordering::SeqCst);
                }));
            }
            let canceller = tokio::spawn(async move {
                cancellation.cancel();
            });
            for waiter in waiters {
                waiter.await.unwrap();
            }
            canceller.await.unwrap();
            assert_eq!(count.load(Ordering::SeqCst), 4);
        });
    }

    pub fn parent_and_owner_cancellation() {
        shuttle::future::block_on(async {
            // Each controller must wake the request on its own; the concurrent
            // case alone could let one working path mask a broken other path.
            for (cancel_parent, close_owner, expected) in [
                (true, false, RocksError::Cancelled),
                (false, true, RocksError::Closed),
                (true, true, RocksError::Closed),
            ] {
                let parent = request();
                let closing = Cancellation::default();
                let child = parent.child(closing.clone());
                let bounded = child.with_timeout(Duration::from_secs(100)).unwrap();
                assert!(bounded.deadline <= child.deadline);
                assert!(bounded.remaining_timeout().unwrap() <= Duration::from_secs(100));
                let observer = child.clone();
                let waiter = tokio::spawn(async move { observer.stopped().await });
                let cancel = tokio::spawn(async move {
                    if cancel_parent {
                        parent.cancel();
                    }
                });
                let close = tokio::spawn(async move {
                    if close_owner {
                        closing.cancel();
                    }
                });
                let reason = waiter.await.unwrap();
                if cancel_parent && close_owner {
                    assert!(matches!(reason, RocksError::Cancelled | RocksError::Closed));
                } else {
                    assert_eq!(reason, expected);
                }
                cancel.await.unwrap();
                close.await.unwrap();
                assert_eq!(child.check(), Err(expected));
            }
        });
    }

    pub fn stage_watch_cancellation() {
        shuttle::future::block_on(async {
            let (parent, parent_signal) = watch::channel(false);
            let (stage, stage_signal) = watch::channel(false);
            let request = request();
            let controlled = request
                .with_cancellation_signals([parent_signal, stage_signal], request.deadline)
                .unwrap();
            let waiter = tokio::spawn(async move { controlled.stopped().await });
            // Closing an unchanged parent signal must not hide the stage signal.
            let close_parent = tokio::spawn(async move {
                drop(parent);
            });
            let cancel_stage = tokio::spawn(async move {
                stage.send(true).unwrap();
            });
            assert_eq!(waiter.await.unwrap(), RocksError::Cancelled);
            close_parent.await.unwrap();
            cancel_stage.await.unwrap();
        });
    }

    pub fn drain_waits_for_completion() {
        shuttle::future::block_on(async {
            let group = Arc::new(StageGroup::default());
            let count = Arc::new(AtomicUsize::new(0));
            let mut workers = Vec::new();
            for _ in 0..3 {
                let mut registration = group.register().unwrap();
                let count = Arc::clone(&count);
                workers.push(tokio::spawn(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    registration.complete(true);
                    drop(registration);
                }));
            }
            group.close_and_drain(&request(), true).await.unwrap();
            assert_eq!(
                count.load(Ordering::SeqCst),
                3,
                "SHUTTLE_ASSERT_DRAIN_WAITED"
            );
            assert!(matches!(group.register(), Err(RocksError::Transition)));
            for worker in workers {
                worker.await.unwrap();
            }
        });
    }

    pub fn registration_races_close() {
        shuttle::future::block_on(async {
            let group = Arc::new(StageGroup::default());
            let late = Arc::clone(&group);
            let worker = tokio::spawn(async move {
                match late.register() {
                    Ok(mut registration) => {
                        registration.complete(true);
                        drop(registration);
                    }
                    Err(error) => assert_eq!(error, RocksError::Transition),
                }
            });
            group.close_and_drain(&request(), true).await.unwrap();
            worker.await.unwrap();
            assert!(matches!(group.register(), Err(RocksError::Transition)));
        });
    }

    pub fn failed_worker_is_retained() {
        shuttle::future::block_on(async {
            let group = Arc::new(StageGroup::default());
            let registration = group.register().unwrap();
            let worker = tokio::spawn(async move {
                drop(registration);
            });
            assert_eq!(
                group.close_and_drain(&request(), true).await,
                Err(RocksError::Transition)
            );
            worker.await.unwrap();
            group.close_and_drain(&request(), false).await.unwrap();
        });
    }

    pub fn cancelled_drain_can_be_retried() {
        shuttle::future::block_on(async {
            let group = Arc::new(StageGroup::default());
            let mut registration = group.register().unwrap();
            let (release, released) = oneshot::channel();
            let worker = tokio::spawn(async move {
                released.await.unwrap();
                registration.complete(true);
                drop(registration);
            });
            let request = request();
            let cancelled = request.clone();
            let cancel = tokio::spawn(async move {
                cancelled.cancel();
            });
            assert_eq!(
                group.close_and_drain(&request, true).await,
                Err(RocksError::Cancelled)
            );
            cancel.await.unwrap();
            assert!(matches!(group.register(), Err(RocksError::Transition)));
            let retry_request = super::runtime::request();
            let retry = group.close_and_drain(&retry_request, true);
            tokio::pin!(retry);
            poll_fn(|cx| {
                assert!(
                    retry.as_mut().poll(cx).is_pending(),
                    "SHUTTLE_ASSERT_DRAIN_WAITED"
                );
                Poll::Ready(())
            })
            .await;
            release.send(()).unwrap();
            worker.await.unwrap();
            retry.await.unwrap();
        });
    }

    pub fn abandoned_wait_can_be_retried() {
        shuttle::future::block_on(async {
            let group = Arc::new(StageGroup::default());
            let mut registration = group.register().unwrap();
            let first_request = request();
            {
                let first = group.close_and_drain(&first_request, true);
                tokio::pin!(first);
                poll_fn(|cx| {
                    assert!(
                        first.as_mut().poll(cx).is_pending(),
                        "SHUTTLE_ASSERT_DRAIN_WAITED"
                    );
                    Poll::Ready(())
                })
                .await;
            }
            assert!(matches!(group.register(), Err(RocksError::Transition)));
            let worker = tokio::spawn(async move {
                registration.complete(true);
                drop(registration);
            });
            group.close_and_drain(&request(), true).await.unwrap();
            worker.await.unwrap();
        });
    }
}

const CASES: [(&str, fn()); 8] = [
    ("cancellation_wakes_all", runtime::cancellation_wakes_all),
    (
        "parent_and_owner_cancellation",
        runtime::parent_and_owner_cancellation,
    ),
    (
        "stage_watch_cancellation",
        runtime::stage_watch_cancellation,
    ),
    (
        "drain_waits_for_completion",
        runtime::drain_waits_for_completion,
    ),
    (
        "registration_races_close",
        runtime::registration_races_close,
    ),
    (
        "failed_worker_is_retained",
        runtime::failed_worker_is_retained,
    ),
    (
        "cancelled_drain_can_be_retried",
        runtime::cancelled_drain_can_be_retried,
    ),
    (
        "abandoned_wait_can_be_retried",
        runtime::abandoned_wait_can_be_retried,
    ),
];

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let selected = match args.as_slice() {
        [] => CASES.to_vec(),
        [name] | [name, _] => vec![
            *CASES
                .iter()
                .find(|(case, _)| case == name)
                .expect("known case"),
        ],
        _ => panic!("expected optional case and replay schedule"),
    };
    let mut config = Config::default();
    config.max_steps = MaxSteps::FailAfter(10_000);
    config.max_time = None;
    for (name, case) in &selected {
        println!("SHUTTLE_CASE {name}");
        if let [_, schedule] = args.as_slice() {
            assert!(
                schedule.len() <= 4096 && schedule.bytes().all(|byte| byte.is_ascii_hexdigit())
            );
            let count =
                Runner::new(ReplayScheduler::new_from_encoded(schedule), config.clone()).run(*case);
            assert_eq!(count, 1, "incomplete replay");
            println!("SHUTTLE_REPLAY_PASSED {name}");
            continue;
        }
        let index = CASES
            .iter()
            .position(|(candidate, _)| candidate == name)
            .unwrap();
        let seed = 0x405_2026 + u64::try_from(index).unwrap();
        let random =
            Runner::new(RandomScheduler::new_from_seed(seed, 1000), config.clone()).run(*case);
        assert_eq!(random, 1000, "incomplete random run");
        let pct =
            Runner::new(PctScheduler::new_from_seed(seed, 3, 1000), config.clone()).run(*case);
        assert_eq!(pct, 1000, "incomplete PCT run");
        println!("SHUTTLE_PASSED {name} random={random} pct={pct} seed={seed}");
    }
}
