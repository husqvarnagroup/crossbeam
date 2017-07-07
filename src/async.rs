use std::cell::Cell;
use std::cell::UnsafeCell;
use std::fmt;
use std::marker::PhantomData;
use std::mem;
use std::ptr;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize, fence};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release, Relaxed, SeqCst};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use coco::epoch::{self, Atomic, Owned};
use either::Either;

use super::SendError;
use super::TrySendError;
use super::SendTimeoutError;
use super::RecvError;
use super::TryRecvError;
use super::RecvTimeoutError;

// TODO: Try Dmitry's modified MPSC queue instead of Michael-Scott. Moreover, don't use complex
// synchronization nor pinning if there's a single consumer. Note that Receiver can't be Sync in
// that case. Also, optimize the Sender side if there's only one.
// Note that in SPSC scenario the Receiver doesn't wait if the queue is in inconsistent state.

use blocking;
use blocking::Blocking;

/// A single node in a queue.
struct Node<T> {
    /// The payload. TODO
    value: T,
    /// The next node in the queue.
    next: Atomic<Node<T>>,
}

/// A lock-free multi-producer multi-consumer queue.
#[repr(C)]
pub struct Queue<T> {
    /// Head of the queue.
    head: Atomic<Node<T>>,
    /// Some padding to avoid false sharing.
    _pad0: [u8; 64],
    /// Tail ofthe queue.
    tail: Atomic<Node<T>>,
    /// Some padding to avoid false sharing.
    _pad1: [u8; 64],
    /// TODO
    closed: AtomicBool,

    receivers: Blocking,

    _marker: PhantomData<T>,
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Send> Sync for Queue<T> {}

impl<T> Queue<T> {
    pub fn new() -> Self {
        // Initialize the internal representation of the queue.
        let queue = Queue {
            head: Atomic::null(),
            _pad0: unsafe { mem::uninitialized() },
            tail: Atomic::null(),
            _pad1: unsafe { mem::uninitialized() },
            closed: AtomicBool::new(false),
            receivers: Blocking::new(),
            _marker: PhantomData,
        };

        // Create a sentinel node.
        let node = Owned::new(Node {
            value: unsafe { mem::uninitialized() },
            next: Atomic::null(),
        });

        unsafe {
            epoch::unprotected(|scope| {
                let node = node.into_ptr(scope);
                queue.head.store(node, Relaxed);
                queue.tail.store(node, Relaxed);
            })
        }

        queue
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        if self.closed.load(SeqCst) {
            return Err(TrySendError::Disconnected(value));
        }

        let mut node = Owned::new(Node {
            value: value,
            next: Atomic::null(),
        });

        epoch::pin(|scope| {
            let mut tail = self.tail.load(Acquire, scope);

            loop {
                // Load the node following the tail.
                let t = unsafe { tail.deref() };
                let next = t.next.load(SeqCst, scope);

                match unsafe { next.as_ref() } {
                    None => {
                        // Try installing the new node.
                        match t.next.compare_and_swap_weak_owned(next, node, SeqCst, scope) {
                            Ok(node) => {
                                // Successfully pushed the node!
                                // Tail pointer mustn't fall behind. Move it forward.
                                let _ = self.tail.compare_and_swap(tail, node, AcqRel, scope);

                                self.receivers.wake_one();
                                return Ok(());
                            }
                            Err((next, n)) => {
                                // Failed. The node that actually follows `t` is `next`.
                                tail = next;
                                node = n;
                            }
                        }
                    }
                    Some(n) => {
                        // Tail pointer fell behind. Move it forward.
                        match self.tail.compare_and_swap_weak(tail, next, AcqRel, scope) {
                            Ok(()) => tail = next,
                            Err(t) => tail = t,
                        }
                    }
                }
            }
        })
    }

    pub fn send_until(
        &self,
        mut value: T,
        _: Option<Instant>,
    ) -> Result<(), SendTimeoutError<T>> {
        match self.try_send(value) {
            Ok(()) => Ok(()),
            Err(TrySendError::Disconnected(v)) => Err(SendTimeoutError::Disconnected(v)),
            Err(TrySendError::Full(v)) => unreachable!(),
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        epoch::pin(|scope| {
            let mut head = self.head.load(SeqCst, scope);

            loop {
                let next = unsafe { head.deref().next.load(SeqCst, scope) };

                match unsafe { next.as_ref() } {
                    None => {
                        if self.closed.load(SeqCst) {
                            return Err(TryRecvError::Disconnected);
                        } else {
                            return Err(TryRecvError::Empty);
                        }
                    }
                    Some(n) => {
                        // Try unlinking the head by moving it forward.
                        match self.head.compare_and_swap_weak(head, next, SeqCst, scope) {
                            Ok(_) => unsafe {
                                // The old head may be later freed.
                                epoch::defer_free(head);

                                // The new head holds the popped value (heads are sentinels!).
                                return Ok(ptr::read(&n.value));
                            },
                            Err(h) => head = h,
                        }
                    }
                }
            }
        })
    }

    pub fn recv_until(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError> {
        let mut blocking = false;
        loop {
            let token;
            if blocking {
                token = self.receivers.register();
            }

            match self.try_recv() {
                Ok(v) => return Ok(v),
                Err(TryRecvError::Disconnected) => {
                    return Err(RecvTimeoutError::Disconnected);
                }
                Err(TryRecvError::Empty) => {
                    if blocking {
                        if let Some(end) = deadline {
                            let now = Instant::now();
                            if now >= end {
                                return Err(RecvTimeoutError::Timeout);
                            } else {
                                thread::park_timeout(end - now);
                            }
                        } else {
                            thread::park();
                        }
                    }
                    blocking = !blocking;
                }
            }
        }
    }

    pub fn close(&self) -> bool {
        if !self.closed.swap(true, SeqCst) {
            println!("CLOSE");
            self.receivers.wake_all();
            true
        } else {
            false
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(SeqCst)
    }
}

// TODO: impl Drop

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::thread;

    #[test]
    fn simple() {
        const STEPS: usize = 1_000_000;

        let q = Arc::new(Queue::with_capacity(5));

        let t = {
            let q = q.clone();
            thread::spawn(move || {
                for i in 0..STEPS {
                    q.send(5);
                }
                println!("SEND DONE");
            })
        };

        for _ in 0..STEPS {
            q.recv();
        }
        println!("RECV DONE");

        t.join().unwrap();
    }
}