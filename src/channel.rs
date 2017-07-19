use std::collections::VecDeque;
use std::ptr;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::{self, AtomicBool, AtomicPtr, AtomicUsize};
use std::sync::atomic::Ordering::{AcqRel, Acquire, Release, Relaxed, SeqCst};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use RecvError;
use RecvTimeoutError;
use SendError;
use SendTimeoutError;
use TryRecvError;
use TrySendError;
use async;
use monitor::Monitor;
use sync;
use zero;

// TODO: iterators

pub trait Channel<T> {
    fn try_send(&self, value: T) -> Result<(), TrySendError<T>>;
    fn send_until(&self, value: T, deadline: Option<Instant>) -> Result<(), SendTimeoutError<T>>;

    fn try_recv(&self) -> Result<T, TryRecvError>;
    fn recv_until(&self, deadline: Option<Instant>) -> Result<T, RecvTimeoutError>;

    fn len(&self) -> usize;
    fn is_empty(&self) -> usize;
    fn is_full(&self) -> usize;
    fn capacity(&self) -> Option<usize>;

    fn close(&self) -> bool;
    fn is_closed(&self) -> bool;

    fn subscribe(&self);
    fn unsubscribe(&self);
    fn is_ready(&self) -> bool;
    fn id(&self) -> usize;

    fn send(&self, value: T) -> Result<(), SendError<T>> {
        match self.send_until(value, None) {
            Ok(()) => Ok(()),
            Err(SendTimeoutError::Disconnected(v)) => Err(SendError(v)),
            Err(SendTimeoutError::Timeout(v)) => Err(SendError(v)),
        }
    }

    fn send_timeout(&self, value: T, dur: Duration) -> Result<(), SendTimeoutError<T>> {
        self.send_until(value, Some(Instant::now() + dur))
    }

    fn recv(&self) -> Result<T, RecvError> {
        if let Ok(v) = self.recv_until(None) {
            Ok(v)
        } else {
            Err(RecvError)
        }
    }

    fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        self.recv_until(Some(Instant::now() + dur))
    }
}

enum Flavor<T> {
    Async(async::Queue<T>),
    Sync(sync::Queue<T>),
    Zero(zero::Queue<T>),
}

struct Queue<T> {
    senders: AtomicUsize,
    receivers: AtomicUsize,
    flavor: Flavor<T>,
}

pub struct Sender<T>(Arc<Queue<T>>);

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Send> Sync for Sender<T> {}

impl<T> Sender<T> {
    fn new(q: Arc<Queue<T>>) -> Self {
        q.senders.fetch_add(1, SeqCst);
        Sender(q)
    }

    pub fn send(&self, value: T) -> Result<(), SendError<T>> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.send(value),
            Flavor::Sync(ref q) => q.send(value),
            Flavor::Zero(ref q) => q.send(value),
        }
    }

    pub fn send_timeout(&self, value: T, dur: Duration) -> Result<(), SendTimeoutError<T>> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.send_timeout(value, dur),
            Flavor::Sync(ref q) => q.send_timeout(value, dur),
            Flavor::Zero(ref q) => q.send_timeout(value, dur),
        }
    }

    pub fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.try_send(value),
            Flavor::Sync(ref q) => q.try_send(value),
            Flavor::Zero(ref q) => q.try_send(value),
        }
    }

    pub fn len(&self) -> usize {
        match self.0.flavor {
            Flavor::Async(ref q) => q.len(),
            Flavor::Sync(ref q) => q.len(),
            Flavor::Zero(ref q) => q.len(),
        }
    }

    pub fn is_empty(&self) -> usize {
        match self.0.flavor {
            Flavor::Async(ref q) => q.is_empty(),
            Flavor::Sync(ref q) => q.is_empty(),
            Flavor::Zero(ref q) => q.is_empty(),
        }
    }

    pub fn is_full(&self) -> usize {
        match self.0.flavor {
            Flavor::Async(ref q) => q.is_full(),
            Flavor::Sync(ref q) => q.is_full(),
            Flavor::Zero(ref q) => q.is_full(),
        }
    }

    pub fn capacity(&self) -> Option<usize> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.capacity(),
            Flavor::Sync(ref q) => q.capacity(),
            Flavor::Zero(ref q) => q.capacity(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.0.senders.fetch_sub(1, SeqCst) == 1 {
            match self.0.flavor {
                Flavor::Async(ref q) => q.close(),
                Flavor::Sync(ref q) => q.close(),
                Flavor::Zero(ref q) => q.close(),
            };
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender::new(self.0.clone())
    }
}

pub struct Receiver<T>(Arc<Queue<T>>);

unsafe impl<T: Send> Send for Receiver<T> {}
unsafe impl<T: Send> Sync for Receiver<T> {}

impl<T> Receiver<T> {
    fn new(q: Arc<Queue<T>>) -> Self {
        q.receivers.fetch_add(1, SeqCst);
        Receiver(q)
    }

    pub(crate) fn as_channel(&self) -> &Channel<T> {
        match self.0.flavor {
            Flavor::Async(ref q) => q,
            Flavor::Sync(ref q) => q,
            Flavor::Zero(ref q) => unimplemented!(),
        }
    }

    pub fn recv(&self) -> Result<T, RecvError> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.recv(),
            Flavor::Sync(ref q) => q.recv(),
            Flavor::Zero(ref q) => q.recv(),
        }
    }

    pub fn recv_timeout(&self, dur: Duration) -> Result<T, RecvTimeoutError> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.recv_timeout(dur),
            Flavor::Sync(ref q) => q.recv_timeout(dur),
            Flavor::Zero(ref q) => q.recv_timeout(dur),
        }
    }

    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.try_recv(),
            Flavor::Sync(ref q) => q.try_recv(),
            Flavor::Zero(ref q) => q.try_recv(),
        }
    }

    pub fn len(&self) -> usize {
        match self.0.flavor {
            Flavor::Async(ref q) => q.len(),
            Flavor::Sync(ref q) => q.len(),
            Flavor::Zero(ref q) => q.len(),
        }
    }

    pub fn is_empty(&self) -> usize {
        match self.0.flavor {
            Flavor::Async(ref q) => q.is_empty(),
            Flavor::Sync(ref q) => q.is_empty(),
            Flavor::Zero(ref q) => q.is_empty(),
        }
    }

    pub fn is_full(&self) -> usize {
        match self.0.flavor {
            Flavor::Async(ref q) => q.is_full(),
            Flavor::Sync(ref q) => q.is_full(),
            Flavor::Zero(ref q) => q.is_full(),
        }
    }

    pub fn capacity(&self) -> Option<usize> {
        match self.0.flavor {
            Flavor::Async(ref q) => q.capacity(),
            Flavor::Sync(ref q) => q.capacity(),
            Flavor::Zero(ref q) => q.capacity(),
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        if self.0.receivers.fetch_sub(1, SeqCst) == 1 {
            match self.0.flavor {
                Flavor::Async(ref q) => q.close(),
                Flavor::Sync(ref q) => q.close(),
                Flavor::Zero(ref q) => q.close(),
            };
        }
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Receiver::new(self.0.clone())
    }
}

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let q = Arc::new(Queue {
        senders: AtomicUsize::new(0),
        receivers: AtomicUsize::new(0),
        flavor: Flavor::Async(async::Queue::new()),
    });
    (Sender::new(q.clone()), Receiver::new(q))
}

pub fn bounded<T>(size: usize) -> (Sender<T>, Receiver<T>) {
    let q = Arc::new(Queue {
        senders: AtomicUsize::new(0),
        receivers: AtomicUsize::new(0),
        flavor: if size == 0 {
            Flavor::Zero(zero::Queue::new())
        } else {
            Flavor::Sync(sync::Queue::with_capacity(size))
        },
    });
    (Sender::new(q.clone()), Receiver::new(q))
}
