// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Release what a multi-event gesture pressed, on every exit.
//!
//! A drag posts a button down, then moves, then the up; a chord posts
//! modifier downs, the key, then the ups. Event creation can fail between the
//! down and the up, and a `?` there returned with the button or key still
//! held: in the foreground that is the user's own mouse button or Command
//! key, stuck until they press it again. A [`ReleaseGuard`] armed right after
//! the press runs its release when it is dropped, so an early return, an
//! error or a panic still lets go. The normal path ends the gesture itself
//! and calls [`ReleaseGuard::disarm`] (or [`ReleaseGuard::release`]).

use std::marker::PhantomData;

/// Runs `release` once, when dropped, unless disarmed first.
#[must_use = "a guard dropped at once releases at once"]
pub struct ReleaseGuard<T, F: FnOnce() -> T> {
    release: Option<F>,
    _result: PhantomData<fn() -> T>,
}

impl<T, F: FnOnce() -> T> ReleaseGuard<T, F> {
    /// Arm a guard right after the press it undoes was posted.
    pub fn new(release: F) -> Self {
        Self {
            release: Some(release),
            _result: PhantomData,
        }
    }

    /// Release now and return the release's own result.
    pub fn release(mut self) -> T {
        let release = self
            .release
            .take()
            .expect("an armed guard holds its release");
        release()
    }

    /// The gesture released through its own last event; nothing is left held.
    pub fn disarm(mut self) {
        self.release = None;
    }
}

impl<T, F: FnOnce() -> T> Drop for ReleaseGuard<T, F> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A gesture that presses, then fails before its own release.
    fn failing_gesture(log: &RefCell<Vec<&'static str>>) -> Result<(), &'static str> {
        log.borrow_mut().push("down");
        let _held = ReleaseGuard::new(|| log.borrow_mut().push("up"));
        Err("event creation failed")?;
        unreachable!()
    }

    #[test]
    fn an_early_error_still_releases() {
        let log = RefCell::new(Vec::new());
        assert!(failing_gesture(&log).is_err());
        assert_eq!(*log.borrow(), ["down", "up"]);
    }

    #[test]
    fn a_panic_still_releases() {
        let log = std::sync::Mutex::new(Vec::new());
        let outcome = std::panic::catch_unwind(|| {
            log.lock().unwrap().push("down");
            let _held = ReleaseGuard::new(|| log.lock().unwrap().push("up"));
            panic!("gesture panicked");
        });
        assert!(outcome.is_err());
        assert_eq!(*log.lock().unwrap(), ["down", "up"]);
    }

    #[test]
    fn a_disarmed_guard_does_not_release_twice() {
        let log = RefCell::new(Vec::new());
        let held = ReleaseGuard::new(|| log.borrow_mut().push("guard up"));
        log.borrow_mut().push("own up");
        held.disarm();
        assert_eq!(*log.borrow(), ["own up"]);
    }

    #[test]
    fn release_runs_once_and_returns_its_result() {
        let count = RefCell::new(0);
        let held = ReleaseGuard::new(|| {
            *count.borrow_mut() += 1;
            Err::<(), _>("up failed")
        });
        assert_eq!(held.release(), Err("up failed"));
        assert_eq!(*count.borrow(), 1);
    }
}
