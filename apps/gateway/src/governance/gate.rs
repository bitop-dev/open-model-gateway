//! The replica's admission gate, shared fairly between workspaces (#876).
//!
//! An admission holds one of the gate's permits from before its transaction
//! begins to its end. Every admission of a workspace locks that workspace's
//! own totals, minute and in-flight rows (`omg_lock_scope_rows`), so a
//! workspace's admissions run one at a time past those locks however many
//! permits they hold. With one queue for all, a burst on one workspace that
//! reached the gate first held every permit while only one of its admissions
//! could make progress, and every other workspace on the replica waited for
//! the whole burst.
//!
//! So waiters queue per workspace, in arrival order, and a freed permit goes
//! to the waiting workspace holding the fewest permits; among those holding
//! equally few, to the one whose first waiter has waited longest. A workspace
//! on its own is served as before: in arrival order, up to every permit.
//!
//! No waiter is passed over for ever: a workspace that waits is never given
//! fewer permits than one that is served before it, so once its own holders
//! end (admissions are bounded) it holds none, and then only workspaces that
//! hold none and whose first waiter arrived earlier go first, of which there
//! are finitely many.
//!
//! Cost per admission: one mutex hold and one map lookup to take a permit
//! and one to give it back. A request that does not wait allocates nothing
//! (the map keeps its capacity). One that waits allocates its wake-up
//! channel, and the waiting workspaces are kept in grant order (`order`), so
//! queueing, leaving the queue and each grant cost O(log n) in the waiting
//! workspaces and their waiters, never a scan.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use tokio::sync::oneshot;
use uuid::Uuid;

pub(crate) struct FairGate {
    state: Mutex<State>,
}

struct State {
    free: usize,
    /// Arrival order of waiters, across workspaces.
    arrivals: u64,
    /// A workspace's entry exists only while it has a holder or a waiter.
    scopes: HashMap<Uuid, Scope>,
    /// The workspaces with a waiter, in grant order: (permits held, its
    /// first waiter's arrival, id). Each one's key is kept current by
    /// [`State::update`], the only place a scope changes.
    order: BTreeSet<(usize, u64, Uuid)>,
}

#[derive(Default)]
struct Scope {
    held: usize,
    /// Waiters by arrival.
    waiters: BTreeMap<u64, oneshot::Sender<()>>,
}

impl Scope {
    fn key(&self, id: Uuid) -> Option<(usize, u64, Uuid)> {
        self.waiters
            .first_key_value()
            .map(|(arrival, _)| (self.held, *arrival, id))
    }
}

impl FairGate {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(State {
                free: permits.max(1),
                arrivals: 0,
                scopes: HashMap::new(),
                order: BTreeSet::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A permit for an admission of `workspace`, waited for until
    /// `deadline`; `None` past it. A dropped wait gives its place back at
    /// once, and a permit is given back when it is dropped.
    pub(crate) async fn acquire(
        &self,
        workspace: Uuid,
        deadline: tokio::time::Instant,
    ) -> Option<GatePermit<'_>> {
        let (arrival, woken) = {
            let mut state = self.lock();
            // Free permits only exist while nobody waits (a release hands a
            // freed permit straight to a waiter), so taking one never passes
            // a waiter.
            if state.free > 0 {
                state.free -= 1;
                state.update(workspace, |s| s.held += 1);
                return Some(GatePermit {
                    gate: self,
                    workspace,
                });
            }
            state.arrivals += 1;
            let arrival = state.arrivals;
            let (wake, woken) = oneshot::channel();
            state.update(workspace, |s| s.waiters.insert(arrival, wake));
            (arrival, woken)
        };
        let mut wait = Wait {
            gate: self,
            workspace,
            arrival,
            woken,
            done: false,
        };
        match tokio::time::timeout_at(deadline, &mut wait.woken).await {
            Ok(Ok(())) => {
                wait.done = true;
                Some(GatePermit {
                    gate: self,
                    workspace,
                })
            }
            // A waiter leaves the queue only by a grant or by its own drop
            // (below), so its sender is never dropped unsent while it waits;
            // if it were, nothing was granted and nothing is given back.
            Ok(Err(_)) => {
                wait.done = true;
                None
            }
            // Past the deadline: the drop leaves the queue, or gives back a
            // permit granted meanwhile.
            Err(_) => None,
        }
    }

    /// Give back a permit of `workspace`, and hand freed permits to waiters.
    fn release(&self, workspace: Uuid) {
        let mut state = self.lock();
        state.update(workspace, |s| {
            debug_assert!(s.held > 0, "a released permit was held");
            s.held = s.held.saturating_sub(1);
        });
        state.free += 1;
        state.dispatch();
    }

    /// Workspaces with an entry (a holder or a waiter).
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        let state = self.lock();
        state.check();
        state.scopes.len()
    }

    /// Permits nobody holds.
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        let state = self.lock();
        state.check();
        state.free
    }
}

impl State {
    /// Change `workspace`'s entry (created if absent) with `change`, keep
    /// its place in `order` current, and drop the entry once it has neither
    /// a holder nor a waiter.
    fn update<R>(&mut self, workspace: Uuid, change: impl FnOnce(&mut Scope) -> R) -> R {
        let scope = self.scopes.entry(workspace).or_default();
        if let Some(key) = scope.key(workspace) {
            self.order.remove(&key);
        }
        let result = change(scope);
        if let Some(key) = scope.key(workspace) {
            self.order.insert(key);
        }
        if scope.held == 0 && scope.waiters.is_empty() {
            self.scopes.remove(&workspace);
        }
        result
    }

    /// Hand each free permit to the first waiter of the waiting workspace
    /// holding the fewest permits (ties: its first waiter arrived first). A
    /// waiter whose receiver is gone is skipped.
    fn dispatch(&mut self) {
        while self.free > 0 {
            let Some(&(_, _, workspace)) = self.order.first() else {
                return;
            };
            let granted = self.update(workspace, |s| {
                let (_, wake) = s.waiters.pop_first().expect("an ordered scope waits");
                let granted = wake.send(()).is_ok();
                if granted {
                    s.held += 1;
                }
                granted
            });
            if granted {
                self.free -= 1;
            }
        }
    }

    /// `order` holds exactly the waiting workspaces under their current
    /// keys, no entry is idle, and free permits never coexist with waiters.
    #[cfg(test)]
    fn check(&self) {
        let expected: BTreeSet<_> = self
            .scopes
            .iter()
            .filter_map(|(id, s)| s.key(*id))
            .collect();
        assert_eq!(self.order, expected, "grant order matches the waiters");
        assert!(
            self.scopes
                .values()
                .all(|s| s.held > 0 || !s.waiters.is_empty()),
            "no idle entry"
        );
        assert!(
            self.free == 0 || self.order.is_empty(),
            "free permits while workspaces wait"
        );
    }
}

/// A wait for a permit. Dropped before it was granted (the deadline passed
/// or the request went away), it leaves the queue; granted but not taken, it
/// gives the permit back.
struct Wait<'a> {
    gate: &'a FairGate,
    workspace: Uuid,
    arrival: u64,
    woken: oneshot::Receiver<()>,
    done: bool,
}
impl Drop for Wait<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Still queued: leave the queue. Not queued any more: a release
        // handed it a permit, which is given back.
        let queued = self
            .gate
            .lock()
            .update(self.workspace, |s| s.waiters.remove(&self.arrival))
            .is_some();
        if !queued {
            self.gate.release(self.workspace);
        }
    }
}

/// A held permit of the gate; given back when dropped.
pub(crate) struct GatePermit<'a> {
    gate: &'a FairGate,
    workspace: Uuid,
}
impl Drop for GatePermit<'_> {
    fn drop(&mut self) {
        self.gate.release(self.workspace);
    }
}
