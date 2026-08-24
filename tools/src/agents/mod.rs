//! Run an external agent, classify what came back, produce a typed result.
//!
//! What both phases need before a CLI can be invoked at all: the model and build to ask
//! for, the shell recipe that asks, the process exit it left behind, and the isolated tree
//! it ran in. All of it lived in `translate.rs`, which is why `verify.rs` had to name the
//! translation module to reach its OWN work tree, and why three of the module cycles ran
//! through that one file.

pub mod exit;
pub mod invocation;
pub mod opencode;
pub mod run;
pub mod session;
pub mod work;

use std::sync::{Condvar, Mutex};

/// THE run's concurrency budget, minted once and lent to every phase. `parallel: usize` was the defect:
/// six sites re-derived a number into their own pool, so `run`'s per-unit loop gave each call an N-wide
/// pool holding one job (`spec-27.md`). A borrower cannot mint a second: `new` below is private.
pub struct Pool(Semaphore);

impl Pool {
    /// Called ONCE per run, from `main`.
    pub fn for_run(parallel: usize) -> Self {
        Self(Semaphore::new(parallel))
    }

    pub fn acquire(&self) -> SemaphoreGuard<'_> {
        self.0.acquire()
    }
}

pub struct Semaphore {
    state: Mutex<usize>,
    cvar: Condvar,
    max: usize,
}

impl Semaphore {
    fn new(max: usize) -> Self {
        Self {
            state: Mutex::new(0),
            cvar: Condvar::new(),
            max,
        }
    }
    /// Poison is recovered, not propagated: a `usize` is never half-updated and the
    /// panicking worker's guard still ran, so the count is sound — while propagating
    /// would panic every sibling worker that next acquires.
    pub fn acquire(&self) -> SemaphoreGuard<'_> {
        let mut count = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *count >= self.max {
            count = self.cvar.wait(count).unwrap_or_else(|e| e.into_inner());
        }
        *count += 1;
        SemaphoreGuard(self)
    }
}

pub struct SemaphoreGuard<'a>(&'a Semaphore);

impl Drop for SemaphoreGuard<'_> {
    fn drop(&mut self) {
        // Runs while unwinding, where a second panic aborts the process; see `acquire`.
        *self.0.state.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
        self.0.cvar.notify_one();
    }
}
