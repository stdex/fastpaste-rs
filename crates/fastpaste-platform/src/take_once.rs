//! A channel whose [`Receiver`](std::sync::mpsc::Receiver) is handed out
//! exactly once.
//!
//! `std::sync::mpsc::Receiver` is `Send` but neither `Sync` nor `Clone`,
//! so an event-source type that must be `Send + Sync` (the `Clipboard`,
//! `GlobalHotkey` traits and their implementations) can't store and
//! re-issue one directly. The contract those implementations expose:
//! the first `changes()`/`events()` call gets the real receiver, later
//! calls get a *disconnected* channel — this helper implements it once.
//!
//! "Disconnected", not "silent": the spare channel's sender is dropped
//! immediately, so `recv()` on it returns `Err(Disconnected)` at once
//! rather than blocking forever. That is the safer of the two (a consumer
//! loop exits instead of hanging), but it does mean a second `take()`
//! shows up as a worker that finishes instantly. Use [`TakeOnceChannel::
//! try_take`] when the caller can tell the difference.

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
    /// fresh disconnected channel (see the module docs).
    ///
    /// A second call is a programming error — the caller has two
    /// consumers where the design allows one — so it is logged. Prefer
    /// [`Self::try_take`] where the caller can handle it.
    pub fn take(&self) -> Receiver<T> {
        self.try_take().unwrap_or_else(|| {
            tracing::warn!(
                "TakeOnceChannel::take called more than once; \
                 the extra receiver is disconnected and will never deliver"
            );
            channel::<T>().1
        })
    }

    /// The real receiver on the first call, `None` on every later one.
    pub fn try_take(&self) -> Option<Receiver<T>> {
        self.rx.lock().unwrap_or_else(|e| e.into_inner()).take()
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
    fn second_take_is_disconnected_not_merely_silent() {
        let ch: TakeOnceChannel<u32> = TakeOnceChannel::new();
        let _first = ch.take();
        let second = ch.take();
        ch.sender().send(1).unwrap();
        // Assert the *specific* error: the spare channel drops its sender
        // immediately, so this returns at once rather than timing out.
        assert_eq!(
            second.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected),
            "the later receiver must be disconnected, not just quiet"
        );
    }

    #[test]
    fn try_take_reports_the_second_attempt() {
        let ch: TakeOnceChannel<u32> = TakeOnceChannel::new();
        assert!(ch.try_take().is_some());
        assert!(ch.try_take().is_none());
    }

    /// Exactly one of many racing takers may win. Nothing else in the
    /// crate exercises the lock under contention.
    #[test]
    fn concurrent_takes_hand_out_exactly_one_receiver() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let ch: Arc<TakeOnceChannel<u32>> = Arc::new(TakeOnceChannel::new());
        let winners = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let ch = Arc::clone(&ch);
            let winners = Arc::clone(&winners);
            handles.push(std::thread::spawn(move || {
                if ch.try_take().is_some() {
                    winners.fetch_add(1, Ordering::SeqCst);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::SeqCst), 1);
    }

    /// A sender racing a take must not lose the message.
    #[test]
    fn sender_racing_take_still_delivers() {
        use std::sync::Arc;
        let ch: Arc<TakeOnceChannel<u32>> = Arc::new(TakeOnceChannel::new());
        let tx = ch.sender();
        let feeder = std::thread::spawn(move || {
            for i in 0..100 {
                let _ = tx.send(i);
            }
        });
        let rx = ch.take();
        feeder.join().unwrap();
        let got: Vec<u32> = rx.try_iter().collect();
        assert_eq!(got.len(), 100, "every send before and after take arrives");
        assert_eq!(got[0], 0);
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
