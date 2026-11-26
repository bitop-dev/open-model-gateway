//! Scan slots and the refusal limiter, on paused time (no wall-clock bound).
use super::*;

fn slots() -> Arc<ScanSlots> {
    ScanSlots::new(SLOTS, SHARE, WAITERS, WAIT)
}

#[tokio::test(start_paused = true)]
async fn a_workspace_holds_its_share_and_a_bounded_number_of_waiters() {
    let pool = slots();
    let flood = Uuid::now_v7();
    let mut held = Vec::new();
    for _ in 0..SHARE {
        held.push(pool.acquire(flood).await.unwrap());
    }
    // More requests of the flooding workspace wait for its share ...
    let waiting: Vec<_> = (SHARE..WAITERS)
        .map(|_| {
            let pool = Arc::clone(&pool);
            tokio::spawn(async move { pool.acquire(flood).await.map(drop) })
        })
        .collect();
    tokio::task::yield_now().await;
    // ... and beyond its bound of waiters it is refused at once.
    let started = tokio::time::Instant::now();
    assert_eq!(pool.acquire(flood).await.err(), Some(NoSlot));
    assert_eq!(started.elapsed(), Duration::ZERO, "refused without waiting");
    // Another workspace is not delayed by it.
    let other = pool.acquire(Uuid::now_v7()).await;
    assert!(other.is_ok());
    assert_eq!(
        started.elapsed(),
        Duration::ZERO,
        "the other workspace waited"
    );
    // The waiters give up after the bounded wait, never the request deadline.
    for w in waiting {
        assert_eq!(w.await.unwrap(), Err(NoSlot));
    }
    assert_eq!(started.elapsed(), WAIT);
    drop(held);
    // Every request left the step: the workspace has its whole bound again.
    let again: Vec<_> =
        futures_util::future::join_all((0..SHARE).map(|_| pool.acquire(flood))).await;
    assert!(again.iter().all(Result::is_ok));
}

#[tokio::test(start_paused = true)]
async fn a_full_pool_refuses_after_the_bounded_wait_and_others_pools_are_untouched() {
    let local = slots();
    let shared = slots();
    let mut held = Vec::new();
    for _ in 0..SLOTS / SHARE {
        let ws = Uuid::now_v7();
        for _ in 0..SHARE {
            held.push(local.acquire(ws).await.unwrap());
        }
    }
    assert_eq!(local.available(), 0);
    let started = tokio::time::Instant::now();
    assert_eq!(local.acquire(Uuid::now_v7()).await.err(), Some(NoSlot));
    assert_eq!(started.elapsed(), WAIT);
    // The shared pool serves at once.
    let started = tokio::time::Instant::now();
    assert!(shared.acquire(Uuid::now_v7()).await.is_ok());
    assert_eq!(started.elapsed(), Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn a_slot_moved_into_work_is_held_until_the_work_ends() {
    let pool = ScanSlots::new(1, 1, 2, WAIT);
    let ws = Uuid::now_v7();
    let slot = pool.acquire(ws).await.unwrap();
    let (release, wait) = std::sync::mpsc::channel::<()>();
    let work = tokio::task::spawn_blocking(move || {
        let _slot = slot;
        wait.recv().unwrap();
    });
    assert_eq!(pool.available(), 0);
    release.send(()).unwrap();
    work.await.unwrap();
    assert_eq!(pool.available(), 1);
}

#[test]
fn refusals_are_counted_per_key_and_minute_within_a_bounded_map() {
    let limiter = RefusalLimiter::new(3, 2);
    let (a, b, c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let now = 600 + 15;
    for _ in 0..3 {
        assert_eq!(limiter.check(a, now), Ok(()));
        limiter.note(a, now);
    }
    assert_eq!(limiter.check(a, now), Err(45), "until the minute ends");
    assert_eq!(
        limiter.check(b, now),
        Ok(()),
        "another key is not throttled"
    );
    limiter.note(b, now);
    // The map is full: a third key is not tracked (and not throttled).
    for _ in 0..5 {
        limiter.note(c, now);
    }
    assert_eq!(limiter.check(c, now), Ok(()));
    // A new minute starts over.
    assert_eq!(limiter.check(a, now + 45), Ok(()));
}
