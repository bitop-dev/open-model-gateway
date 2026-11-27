//! The admission gate shared fairly between workspaces (#876): a workspace
//! on its own takes every permit, in arrival order; a freed permit goes to the
//! waiting workspace holding the fewest (ties: its first waiter arrived
//! first); no waiter is passed over for ever; a dropped wait or holder gives
//! its place back at once; the deadline ends a wait; and a workspace has an
//! entry only while it has a holder or a waiter.
use super::gate::{FairGate, GatePermit};
use super::*;
use std::task::Poll;
use std::time::Duration;
use tokio::time::Instant;

fn later() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

/// Polls `wait` once: queued (pending), or granted. Outside tokio's
/// cooperative budget: a test that polls many waits in one task would
/// otherwise see a granted one answer `Pending` once the budget ran out.
async fn poll_once<F: std::future::Future + Unpin>(wait: &mut F) -> Poll<F::Output> {
    tokio::task::unconstrained(async { futures_util::poll!(wait) }).await
}

type Pending<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<GatePermit<'a>>> + 'a>>;

fn queue<'a>(gate: &'a FairGate, workspace: Uuid) -> Pending<'a> {
    Box::pin(gate.acquire(workspace, later()))
}

/// Which of `waits` a poll finds granted (they are removed), in order.
async fn granted<'a>(
    waits: &mut Vec<(usize, Pending<'a>)>,
    held: &mut Vec<GatePermit<'a>>,
) -> Vec<usize> {
    let mut ready = Vec::new();
    let mut i = 0;
    while i < waits.len() {
        if let Poll::Ready(permit) = poll_once(&mut waits[i].1).await {
            held.push(permit.expect("granted"));
            ready.push(waits.remove(i).0);
        } else {
            i += 1;
        }
    }
    ready
}

#[tokio::test]
async fn a_workspace_on_its_own_takes_every_permit_in_arrival_order() {
    let gate = FairGate::new(5);
    let workspace = Uuid::new_v4();
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(gate.acquire(workspace, later()).await.unwrap());
    }
    assert_eq!(gate.available(), 0);
    let mut waits: Vec<_> = (0..4).map(|n| (n, queue(&gate, workspace))).collect();
    for (_, wait) in &mut waits {
        assert!(poll_once(wait).await.is_pending());
    }
    let mut order = Vec::new();
    for _ in 0..4 {
        drop(held.remove(0));
        order.extend(granted(&mut waits, &mut held).await);
    }
    assert_eq!(order, vec![0, 1, 2, 3], "served in arrival order");
    drop(held);
    assert_eq!((gate.available(), gate.tracked()), (5, 0));
}

#[tokio::test]
async fn a_freed_permit_goes_to_the_waiting_workspace_holding_the_fewest() {
    let gate = FairGate::new(5);
    let (hot, other) = (Uuid::new_v4(), Uuid::new_v4());
    let mut held = Vec::new();
    for _ in 0..5 {
        held.push(gate.acquire(hot, later()).await.unwrap());
    }
    // The hot workspace's waiter arrived first; the other holds nothing.
    let mut waits = vec![(0, queue(&gate, hot)), (1, queue(&gate, other))];
    for (_, wait) in &mut waits {
        assert!(poll_once(wait).await.is_pending());
    }
    drop(held.remove(0));
    assert_eq!(
        granted(&mut waits, &mut held).await,
        vec![1],
        "the other workspace first"
    );
    drop(held.remove(0));
    assert_eq!(granted(&mut waits, &mut held).await, vec![0]);
    drop(held);
    assert_eq!((gate.available(), gate.tracked()), (5, 0));
}

#[tokio::test]
async fn among_workspaces_holding_equally_few_the_longest_waiting_goes_first() {
    let gate = FairGate::new(1);
    let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut held = vec![gate.acquire(c, later()).await.unwrap()];
    // `b`'s waiter arrives before `a`'s; both hold nothing.
    let mut waits = vec![(0, queue(&gate, b)), (1, queue(&gate, a))];
    for (_, wait) in &mut waits {
        assert!(poll_once(wait).await.is_pending());
    }
    drop(held.remove(0));
    assert_eq!(granted(&mut waits, &mut held).await, vec![0]);
    drop(held.remove(0));
    assert_eq!(granted(&mut waits, &mut held).await, vec![1]);
    drop(held);
    assert_eq!(gate.tracked(), 0);
}

/// A hot workspace keeps making progress while other workspaces keep
/// arriving (one every other release, a stream the gate can serve), and no
/// waiter is passed over: each release grants one waiter, and the longest
/// any waiter waits, and the longest gap between the hot workspace's grants,
/// stay as short in the second half of the run as in the first.
#[tokio::test]
async fn a_hot_workspace_progresses_while_other_workspaces_keep_arriving() {
    let gate = FairGate::new(3);
    let hot = Uuid::new_v4();
    let mut held = Vec::new();
    for _ in 0..3 {
        held.push(gate.acquire(hot, later()).await.unwrap());
    }
    // (hot?, step it arrived at, its wait)
    let mut waits: Vec<(bool, usize, Pending<'_>)> = vec![(true, 0, queue(&gate, hot))];
    assert!(poll_once(&mut waits[0].2).await.is_pending());
    const STEPS: usize = 240;
    // Per half of the run: (hot grants, longest gap between them, longest wait).
    let mut halves = [(0, 0, 0); 2];
    let mut last_hot = 0;
    for step in 0..STEPS {
        let half = &mut halves[usize::from(step >= STEPS / 2)];
        if step % 2 == 0 {
            let mut newcomer = queue(&gate, Uuid::new_v4());
            assert!(poll_once(&mut newcomer).await.is_pending());
            waits.push((false, step, newcomer));
        }
        // The oldest holder ends: one permit is freed.
        drop(held.remove(0));
        let mut served = 0;
        let mut i = 0;
        while i < waits.len() {
            if let Poll::Ready(permit) = poll_once(&mut waits[i].2).await {
                held.push(permit.expect("granted"));
                let (is_hot, arrived, _) = waits.remove(i);
                half.2 = half.2.max(step - arrived);
                served += 1;
                if is_hot {
                    half.0 += 1;
                    half.1 = half.1.max(step - last_hot);
                    last_hot = step;
                    // Its next request queues at once.
                    let mut next = queue(&gate, hot);
                    assert!(poll_once(&mut next).await.is_pending());
                    waits.push((true, step, next));
                }
            } else {
                i += 1;
            }
        }
        assert_eq!(served, 1, "step {step}: one freed permit, one grant");
    }
    for (_, arrived, _) in &waits {
        halves[1].2 = halves[1].2.max(STEPS - 1 - arrived);
    }
    let [
        (first_hot, first_gap, first_wait),
        (second_hot, second_gap, second_wait),
    ] = halves;
    assert!(first_hot > 0 && second_hot > 0, "{halves:?}");
    assert!(
        second_gap <= first_gap && second_wait <= first_wait,
        "waits grew over the run: {halves:?}"
    );
    assert!(first_gap <= 6 && first_wait <= 6, "{halves:?}");
    drop((waits, held));
    assert_eq!(gate.tracked(), 0);
}

#[tokio::test]
async fn a_dropped_wait_or_holder_gives_its_place_back_at_once() {
    let gate = FairGate::new(2);
    let workspace = Uuid::new_v4();
    let first = gate.acquire(workspace, later()).await.unwrap();
    let second = gate.acquire(workspace, later()).await.unwrap();
    let mut dropped = queue(&gate, workspace);
    let mut behind = queue(&gate, workspace);
    assert!(poll_once(&mut dropped).await.is_pending());
    assert!(poll_once(&mut behind).await.is_pending());
    drop(dropped);
    drop(first);
    let Poll::Ready(Some(behind)) = poll_once(&mut behind).await else {
        panic!("the waiter behind a dropped one is next");
    };
    drop(second);
    assert_eq!(
        gate.available(),
        1,
        "a dropped holder's permit is free at once"
    );
    drop(behind);
    assert_eq!((gate.available(), gate.tracked()), (2, 0));
}

#[tokio::test]
async fn a_wait_granted_but_dropped_before_it_took_the_permit_gives_it_back() {
    let gate = FairGate::new(1);
    let workspace = Uuid::new_v4();
    let holder = gate.acquire(workspace, later()).await.unwrap();
    let mut wait = queue(&gate, workspace);
    assert!(poll_once(&mut wait).await.is_pending());
    // The release hands the permit to the queued wait, which is dropped
    // before it is polled again (its request went away).
    drop(holder);
    drop(wait);
    assert_eq!((gate.available(), gate.tracked()), (1, 0));
}

#[tokio::test]
async fn a_wait_past_its_deadline_gets_no_permit_and_leaves_no_entry() {
    let gate = FairGate::new(1);
    let workspace = Uuid::new_v4();
    let holder = gate.acquire(workspace, later()).await.unwrap();
    let started = std::time::Instant::now();
    let shed = gate
        .acquire(Uuid::new_v4(), Instant::now() + Duration::from_millis(100))
        .await;
    assert!(shed.is_none());
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(gate.tracked(), 1, "only the holder's workspace");
    drop(holder);
    assert_eq!((gate.available(), gate.tracked()), (1, 0));
}

#[tokio::test]
async fn a_dropped_waiter_between_others_leaves_its_workspace_queue_in_order() {
    let gate = FairGate::new(1);
    let workspace = Uuid::new_v4();
    let mut held = vec![gate.acquire(workspace, later()).await.unwrap()];
    let mut waits: Vec<_> = (0..3).map(|n| (n, queue(&gate, workspace))).collect();
    for (_, wait) in &mut waits {
        assert!(poll_once(wait).await.is_pending());
    }
    drop(waits.remove(1));
    assert_eq!(gate.tracked(), 1, "the workspace still holds and waits");
    let mut order = Vec::new();
    for _ in 0..2 {
        drop(held.remove(0));
        order.extend(granted(&mut waits, &mut held).await);
    }
    assert_eq!(order, vec![0, 2]);
    drop(held);
    assert_eq!((gate.available(), gate.tracked()), (1, 0));
}

/// A permit handed over before the deadline is taken even when the waiter
/// is next polled after it.
#[tokio::test(start_paused = true)]
async fn a_permit_granted_before_the_deadline_is_taken_after_it() {
    let gate = FairGate::new(1);
    let holder = gate.acquire(Uuid::new_v4(), later()).await.unwrap();
    let mut wait =
        Box::pin(gate.acquire(Uuid::new_v4(), Instant::now() + Duration::from_millis(100)));
    assert!(poll_once(&mut wait).await.is_pending());
    drop(holder);
    tokio::time::advance(Duration::from_millis(200)).await;
    let Poll::Ready(Some(permit)) = poll_once(&mut wait).await else {
        panic!("the granted permit is taken");
    };
    assert_eq!(gate.available(), 0);
    drop(permit);
    assert_eq!((gate.available(), gate.tracked()), (1, 0));
}

/// Many admissions of a few workspaces on a multi-threaded runtime, with
/// random deadlines, holds, cancellations and aborts: never more holders
/// than permits, and afterwards every permit is free and no entry is left
/// (`available` and `tracked` also check the grant order).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn permits_are_conserved_under_cancellation_and_deadlines() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    const PERMITS: usize = 5;
    let gate = std::sync::Arc::new(FairGate::new(PERMITS));
    let workspaces: Vec<_> = (0..7).map(|_| Uuid::new_v4()).collect();
    let holding = std::sync::Arc::new(AtomicUsize::new(0));
    let most = std::sync::Arc::new(AtomicUsize::new(0));
    struct Holding(std::sync::Arc<AtomicUsize>);
    impl Drop for Holding {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }
    for round in 0..10u64 {
        let mut tasks = Vec::new();
        for n in 0..600u64 {
            // A fixed mix per task, so a failure reproduces.
            let mix = (n * 7919 + round * 104_729) % 1000;
            let (gate, holding, most) = (gate.clone(), holding.clone(), most.clone());
            let workspace = workspaces[(mix % 7) as usize];
            tasks.push(tokio::spawn(async move {
                let deadline = Instant::now() + Duration::from_micros(mix % 3000);
                let work = async {
                    if let Some(permit) = gate.acquire(workspace, deadline).await {
                        let now = holding.fetch_add(1, Ordering::SeqCst) + 1;
                        let _held = Holding(holding.clone());
                        most.fetch_max(now, Ordering::SeqCst);
                        if mix % 2 == 0 {
                            tokio::task::yield_now().await;
                        } else {
                            tokio::time::sleep(Duration::from_micros(mix % 500)).await;
                        }
                        drop(permit);
                    }
                };
                if mix % 4 == 0 {
                    // Cancelled mid-wait or mid-hold.
                    let _ = tokio::time::timeout(Duration::from_micros(mix % 700), work).await;
                } else {
                    work.await;
                }
            }));
        }
        for (n, task) in tasks.into_iter().enumerate() {
            if n % 11 == 0 {
                task.abort();
            }
            let _ = task.await;
        }
        assert_eq!(
            (gate.available(), gate.tracked()),
            (PERMITS, 0),
            "round {round}"
        );
    }
    assert!(most.load(Ordering::SeqCst) <= PERMITS);
}

/// Why a workspace needs few of the gate's permits: every admission of one
/// workspace locks that workspace's own rows (`omg_lock_scope_rows`), so two
/// admissions of it wait for each other there, whichever key or user they
/// use, whatever limits the keys have, in a shared or a personal workspace;
/// admissions of different workspaces do not.
#[cfg(feature = "integration-tests")]
mod rows {
    use super::*;
    use crate::governance::tests::db::fixture;
    use gateway_datetime_compat::UtcInstant;
    use sqlx::PgPool;

    async fn key(pool: &PgPool, workspace: Uuid, user: Uuid) -> Uuid {
        let key = crate::auth::NewApiKey::generate().unwrap();
        sqlx::query("INSERT INTO api_keys(id,workspace_id,issued_to_user_id,name,secret_hash) VALUES($1,$2,$3,'scope rows',$4)")
            .bind(key.id).bind(workspace).bind(user).bind(key.digest.as_slice())
            .execute(pool).await.unwrap();
        key.id
    }

    /// Whether an admission's row locks for `b` wait for those `a` holds.
    async fn waits(pool: &PgPool, a: (Uuid, Uuid), b: (Uuid, Uuid)) -> bool {
        let at = UtcInstant::now();
        let touch = |(workspace, api_key)| locks::Touch {
            workspace,
            api_key,
            at,
        };
        let mut first = pool.begin().await.unwrap();
        locks::rows(&mut first, &[touch(a)], true, None)
            .await
            .unwrap();
        let mut second = pool.begin().await.unwrap();
        let result = locks::rows(
            &mut second,
            &[touch(b)],
            true,
            Some(Duration::from_millis(300)),
        )
        .await;
        second.rollback().await.unwrap();
        first.rollback().await.unwrap();
        match result {
            Ok(()) => false,
            Err(e) if locks::is_lock_timeout(&e) => true,
            Err(e) => panic!("{e}"),
        }
    }

    #[dbtest::test]
    async fn admissions_of_one_workspace_wait_for_each_other_at_its_rows(pool: PgPool) {
        let f = fixture(pool.clone()).await;
        let (team, personal) = (f.team.workspace_id, f.principal.workspace_id);
        let others_team_key = key(&pool, team, f.other).await;
        let owners_second_personal_key = key(&pool, personal, f.owner).await;
        let owners_second_team_key = key(&pool, team, f.owner).await;
        // No limits anywhere yet.
        assert!(
            waits(&pool, (team, f.team.key_id), (team, others_team_key)).await,
            "a team workspace without limits: two users' keys"
        );
        assert!(
            waits(&pool, (team, f.team.key_id), (team, owners_second_team_key)).await,
            "a team workspace without limits: one user's two keys"
        );
        assert!(
            waits(
                &pool,
                (personal, f.principal.key_id),
                (personal, owners_second_personal_key)
            )
            .await,
            "a personal workspace: two keys"
        );
        // Limits on keys only.
        sqlx::query("INSERT INTO key_policies(workspace_id,governance_key_id,requests_per_minute) VALUES($1,$2,1000)")
            .bind(team).bind(f.team.key_id).execute(&pool).await.unwrap();
        assert!(
            waits(&pool, (team, f.team.key_id), (team, others_team_key)).await,
            "limits on one key only"
        );
        // Different workspaces do not wait for each other.
        assert!(
            !waits(&pool, (team, f.team.key_id), (personal, f.principal.key_id)).await,
            "admissions of different workspaces run in parallel"
        );
    }
}
