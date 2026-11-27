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
//! (the map keeps its capacity); one that waits allocates its wake-up
//! channel. A release while workspaces wait scans the waiting workspaces.
use std::collections::{HashMap, VecDeque};
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
}

#[derive(Default)]
struct Scope {
    held: usize,
    waiters: VecDeque<Waiter>,
}

struct Waiter {
    arrival: u64,
    wake: oneshot::Sender<()>,
}

impl FairGate {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            state: Mutex::new(State {
                free: permits.max(1),
                arrivals: 0,
                scopes: HashMap::new(),
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
                state.scopes.entry(workspace).or_default().held += 1;
                return Some(GatePermit {
                    gate: self,
                    workspace,
                });
            }
            state.arrivals += 1;
            let arrival = state.arrivals;
            let (wake, woken) = oneshot::channel();
            state
                .scopes
                .entry(workspace)
                .or_default()
                .waiters
                .push_back(Waiter { arrival, wake });
            (arrival, woken)
        };
        let mut wait = Wait {
            gate: self,
            workspace,
            arrival,
            woken,
            done: false,
        };
        let granted = tokio::time::timeout_at(deadline, &mut wait.woken).await;
        if !matches!(granted, Ok(Ok(()))) {
            return None;
        }
        wait.done = true;
        Some(GatePermit {
            gate: self,
            workspace,
        })
    }

    /// Give back a permit of `workspace`, and hand freed permits to waiters.
    fn release(&self, workspace: Uuid) {
        let mut state = self.lock();
        if let Some(scope) = state.scopes.get_mut(&workspace) {
            scope.held -= 1;
        }
        state.free += 1;
        forget_if_idle(&mut state, workspace);
        state.dispatch();
    }

    /// Workspaces with an entry (a holder or a waiter).
    #[cfg(test)]
    pub(crate) fn tracked(&self) -> usize {
        self.lock().scopes.len()
    }

    /// Permits nobody holds.
    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.lock().free
    }
}

fn forget_if_idle(state: &mut State, workspace: Uuid) {
    if state
        .scopes
        .get(&workspace)
        .is_some_and(|s| s.held == 0 && s.waiters.is_empty())
    {
        state.scopes.remove(&workspace);
    }
}

impl State {
    /// Hand each free permit to the first waiter of the waiting workspace
    /// holding the fewest permits (ties: its first waiter arrived first). A
    /// waiter whose wait already ended is skipped.
    fn dispatch(&mut self) {
        while self.free > 0 {
            let Some(workspace) = self
                .scopes
                .iter()
                .filter_map(|(id, s)| s.waiters.front().map(|w| (s.held, w.arrival, *id)))
                .min()
                .map(|(_, _, id)| id)
            else {
                return;
            };
            let scope = self.scopes.get_mut(&workspace).expect("chosen scope");
            let waiter = scope.waiters.pop_front().expect("chosen waiter");
            if waiter.wake.send(()).is_ok() {
                scope.held += 1;
                self.free -= 1;
            } else {
                forget_if_idle(self, workspace);
            }
        }
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
        let granted = {
            let mut state = self.gate.lock();
            let queued = state.scopes.get_mut(&self.workspace).and_then(|s| {
                let at = s.waiters.iter().position(|w| w.arrival == self.arrival)?;
                s.waiters.remove(at)
            });
            if queued.is_some() {
                forget_if_idle(&mut state, self.workspace);
                false
            } else {
                // Not queued any more: a release handed it a permit.
                true
            }
        };
        if granted {
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
