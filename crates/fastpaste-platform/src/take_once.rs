//! A channel whose [`Receiver`](std::sync::mpsc::Receiver) is handed out
//! exactly once.
//!
//! `std::sync::mpsc::Receiver` is `Send` but neither `Sync` nor `Clone`,
//! so an event-source type that must be `Send + Sync` (the `Clipboard`,
//! `GlobalHotkey` traits and their implementations) can't store and
//! re-issue one directly. The contract those implementations expose:
//! the first `changes()`/`events()` call gets the real receiver, later
//! calls get a fresh forever-silent channel — this helper implements it
//! once.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Single-consumer channel with a take-once receiver handout.
///
/// The sender side is cloneable, so worker threads can keep feeding the
/// channel after the owning struct hands the receiver out.
pub struct TakeOnceChannel<T> {
    tx: Sender<T>,
    rx: Mutex<Option<Receiver<T>>>,
}

impl<T> TakeOnceChannel<T> {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            tx,
            rx: Mutex::new(Some(rx)),
        }
    }

    /// A clone of the sender for worker threads to emit on.
    pub fn sender(&self) -> Sender<T> {
        self.tx.clone()
    }

    /// First call returns the real receiver; every later call returns a
    /// fresh empty channel that never delivers (see the module docs).
    pub fn take(&self) -> Receiver<T> {
        self.rx
            .lock()
            .expect("take-once receiver mutex poisoned")
            .take()
            .unwrap_or_else(|| channel::<T>().1)
    }
}

impl<T> Default for TakeOnceChannel<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_take_receives() {
        let ch: TakeOnceChannel<u32> = TakeOnceChannel::new();
        ch.sender().send(7).unwrap();
        let rx = ch.take();
        assert_eq!(rx.recv(), Ok(7));
    }

    #[test]
    fn second_take_stays_silent() {
        let ch: TakeOnceChannel<u32> = TakeOnceChannel::new();
        let _first = ch.take();
        let second = ch.take();
        ch.sender().send(1).unwrap();
        assert!(
            second
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "later receiver must stay silent forever"
        );
    }

    #[test]
    fn cloned_senders_keep_channel_alive() {
        let ch: TakeOnceChannel<&'static str> = TakeOnceChannel::new();
        let worker_tx = ch.sender();
        let rx = ch.take();
        // Drop the struct: only the worker's sender clone keeps the
        // channel open — its sends must still reach the handed-out
        // receiver.
        drop(ch);
        worker_tx.send("x").unwrap();
        assert_eq!(rx.recv(), Ok("x"));
    }
}
