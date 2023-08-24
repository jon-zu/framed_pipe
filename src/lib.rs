#![doc = include_str!("../README.md")]

use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{ready, Poll},
};

use bytes::{BufMut, Bytes, BytesMut};
use futures::Stream;
use thiserror::Error;
use tokio::sync::mpsc;

// Attempts to reserve additional bytes on the buffer, by respecting the maximum capacity
// TODO: Not working properly for now so waiting on this: https://github.com/tokio-rs/bytes/issues/620
// The problem lies in the fact, that the consumer holds a frame alive, which basically
// denies the buffer to reclaim the memory
// However the behaviour is accept-able assuming there's only a single consumer
// which should hold at most one frame at a time
fn try_reserve(buf: &mut BytesMut, additional: usize, max_cap: usize) -> bool {
    let remaining = max_cap - buf.len();
    if remaining < additional {
        return false;
    }
    buf.reserve(additional);

    true
}

#[derive(Debug, Error)]
pub enum FramedPipeError {
    #[error("Try Send Error: {0}")]
    TrySendError(#[from] mpsc::error::TrySendError<Option<usize>>),
    #[error("Send Error: {0}")]
    SendError(#[from] mpsc::error::SendError<()>),
    #[error("Out of capacity")]
    OutOfCapacity,
    #[error("Capacity limit was reached")]
    CapacityLimitReached,
    /// Signals the reader that this pipe missed frames due to being out of capacity
    #[error("Missed frame")]
    MissedFrame,
}

/// Buffer to store the frames and furhter context
#[derive(Debug, Clone)]
struct FramedPipeBuf {
    buf: BytesMut,
    cap: usize,
}

impl FramedPipeBuf {
    /// Create a new buffer with the given capacity
    fn new(cap: usize) -> Self {
        Self {
            buf: BytesMut::with_capacity(cap),
            cap,
        }
    }

    /// Takes a frame from the buffer
    /// If a frame was missed an Error is returned
    fn take_frame(&mut self, n: usize) -> Result<Bytes, FramedPipeError> {
        let frame = self.buf.split_to(n).freeze();

        Ok(frame)
    }

    /// Attempt to reserve more space, but respect the capacity limit
    fn try_reserve(&mut self, additional: usize) -> bool {
        try_reserve(&mut self.buf, additional, self.cap)
    }

    /// Puts a frame onto the buffer
    fn put_frame(&mut self, frame: &[u8]) {
        self.buf.put_slice(frame)
    }
}

/// Shared handle for Sender and Receiver
type SharedFramedPipeBuf = Arc<Mutex<FramedPipeBuf>>;

/// A sender for the `FramedPipe` can be cloned and used a `Sink`
#[derive(Debug, Clone)]
pub struct FramedPipeSender {
    tx: mpsc::Sender<Option<usize>>,
    buf: SharedFramedPipeBuf,
}

impl FramedPipeSender {
    /// Use try send to attempt to send a frame
    fn try_push(
        frame: &[u8],
        buf: &mut FramedPipeBuf,
        tx: &mut mpsc::Sender<Option<usize>>,
    ) -> Result<(), FramedPipeError> {
        if buf.try_reserve(frame.len()) {
            tx.try_send(Some(frame.as_ref().len()))?;
            buf.put_frame(frame);
            Ok(())
        } else {
            tx.try_send(None)?;
            Err(FramedPipeError::CapacityLimitReached)
        }
    }

    /// Sends a frame onto the pipe
    pub async fn send(&mut self, item: &[u8]) -> Result<(), FramedPipeError> {
        let permit = self.tx.reserve().await?;

        {
            let mut buf = self.buf.lock().expect("mtx");
            if buf.try_reserve(item.len()) {
                buf.put_frame(item);
                permit.send(Some(item.len()));
                Ok(())
            } else {
                permit.send(None);
                Err(FramedPipeError::OutOfCapacity)
            }
        }
    }

    /// Try to send a frame onto the pipe
    pub fn try_send(&mut self, item: &[u8]) -> Result<(), FramedPipeError> {
        let mut buf = self.buf.lock().expect("mtx");
        Self::try_push(item, &mut buf, &mut self.tx)
    }

    /// Try to send all frames onto a pipe
    /// May send send some frames and then cancel
    pub fn try_send_all<B: AsRef<[u8]>>(
        &mut self,
        items: impl Iterator<Item = B>,
    ) -> Result<usize, FramedPipeError> {
        let mut buf = self.buf.lock().expect("mtx");
        let mut n = 0;
        for item in items {
            Self::try_push(item.as_ref(), &mut buf, &mut self.tx)?;
            n += 1;
        }
        Ok(n)
    }
}

/// Receiver end for the `FramedPipe`, there's at most one reader
#[derive(Debug)]
pub struct FramedPipeReceiver {
    rx: mpsc::Receiver<Option<usize>>,
    buf: SharedFramedPipeBuf,
}

impl FramedPipeReceiver {
    /// Closes the pipe
    pub fn close(&mut self) {
        self.rx.close()
    }
}

/// Stream impl for the reader, wait on the channel
impl Stream for FramedPipeReceiver {
    type Item = Result<Bytes, FramedPipeError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        // There's only one reader so we can just wait on the channel
        // and then read the frame of the buffer
        let next_frame = ready!(Pin::new(&mut self.rx).poll_recv(cx));
        Poll::Ready(next_frame.map(|n| match n {
            Some(n) => self.buf.lock().expect("mtx").take_frame(n),
            None => Err(FramedPipeError::MissedFrame),
        }))
    }
}

/// Creates a framed pipe
/// `buf_cap` describes the maximum capacity in bytes for the buffer
/// `frame_cap` describes the maximum capacity in terms of frames
pub fn framed_pipe(buf_cap: usize, frame_cap: usize) -> (FramedPipeSender, FramedPipeReceiver) {
    let buf = Arc::new(Mutex::new(FramedPipeBuf::new(buf_cap)));
    let (tx, rx) = mpsc::channel(frame_cap);

    (
        FramedPipeSender {
            buf: buf.clone(),
            tx,
        },
        FramedPipeReceiver { buf, rx },
    )
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use test_case::test_case;

    use super::*;

    #[test]
    fn reserve_cap() {
        let mut buf = BytesMut::with_capacity(4);
        buf.put_u16(10);
        assert!(try_reserve(&mut buf, 2, 4));
        assert!(try_reserve(&mut buf, 0, 4));
        assert!(!try_reserve(&mut buf, 3, 4));

        // Buffer with higher capacity still respects the limit
        let mut buf = BytesMut::with_capacity(6);
        assert!(!try_reserve(&mut buf, 6, 4));
    }

    // Test with multiple echo data
    #[test_case(256)]
    #[test_case(1024)]
    #[test_case(4096)]
    #[tokio::test]
    async fn echo_pipe(n: usize) {
        let (tx, mut rx) = framed_pipe(n * 2, 4);

        let echo_data = vec![vec![0xFF; n], vec![1, 2], vec![], vec![0x0; n / 2]];

        for _ in 0..100 {
            for data in echo_data.iter() {
                tx.clone().try_send(data).expect("send");
            }

            for data in echo_data.iter() {
                let rx_data = rx.next().await.unwrap().expect("rx");
                assert_eq!(&rx_data, data.as_slice());
            }
        }
    }

    // Test with multiple echo data
    #[test_case(256)]
    #[test_case(1024)]
    #[test_case(4096)]
    #[tokio::test]
    async fn echo_pipe_all(n: usize) {
        let (mut tx, mut rx) = framed_pipe(n * 2, 4);

        let echo_data = vec![vec![0xFF; n], vec![1, 2], vec![], vec![0x0; n / 2]];

        for _ in 0..100 {
            tx.try_send_all(echo_data.iter()).expect("Send");

            for data in echo_data.iter() {
                let rx_data = rx.next().await.unwrap().expect("rx");
                assert_eq!(&rx_data, data.as_slice());
            }
        }
    }

    // Test
    #[test_case(256)]
    #[test_case(1024)]
    #[test_case(4096)]
    #[tokio::test]
    async fn reclaim_echo_pipe(n: usize) {
        let (mut tx, mut rx) = framed_pipe(n, 2);
        let fill_data = vec![0xFF; n];

        // Sending only works once
        tx.try_send(&fill_data).expect("Send");
        // Second send will fail
        assert!(matches!(
            tx.try_send(&fill_data),
            Err(FramedPipeError::CapacityLimitReached)
        ));

        // Frame should appear first
        assert_eq!(rx.next().await.unwrap().expect("recv"), fill_data);
        // Error with missed frame should follow
        assert!(matches!(
            rx.next().await.expect("recv"),
            Err(FramedPipeError::MissedFrame)
        ));

        tx.try_send(&fill_data).expect("Send");
        assert_eq!(rx.next().await.unwrap().expect("recv"), fill_data);
    }
}
