//! Running D-Bus calls concurrently, without a `futures-util` dependency.
//!
//! §4.2's measurements assume `GetExtents` is *pipelined* over survivors:
//! at ~0.08 ms a call, thirty survivors cost 2.4 ms in series and roughly
//! one round trip in parallel, which is the difference between fitting in
//! the 5 ms budget and not.
//!
//! `futures-lite` — already here through zbus — has no `buffered`, and
//! pulling in `futures-util` for one combinator is the kind of dependency
//! `muvor-uictl` exists to avoid. This is that combinator, in thirty lines.

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::task::Poll;

/// How many calls may be outstanding at once.
///
/// Unbounded concurrency would dump several hundred simultaneous method
/// calls on one application, and an app that is slow to answer is exactly
/// the app that must not be flooded. Sixty-four is well past the point where
/// the socket, not the parallelism, is the limit.
pub(crate) const MAX_IN_FLIGHT: usize = 64;

/// Drive every future to completion concurrently, returning results in the
/// order given. Runs at most [`MAX_IN_FLIGHT`] at a time.
pub(crate) async fn join_all<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let mut out = Vec::with_capacity(futures.len());
    let mut chunk = Vec::with_capacity(MAX_IN_FLIGHT.min(futures.len().max(1)));
    for f in futures {
        chunk.push(f);
        if chunk.len() == MAX_IN_FLIGHT {
            out.extend(join_chunk(std::mem::take(&mut chunk)).await);
        }
    }
    if !chunk.is_empty() {
        out.extend(join_chunk(chunk).await);
    }
    out
}

async fn join_chunk<F: Future>(futures: Vec<F>) -> Vec<F::Output> {
    let n = futures.len();
    let mut pending: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
    let mut done: Vec<Option<F::Output>> = (0..n).map(|_| None).collect();
    let mut finished = 0;

    poll_fn(|cx| {
        // Every future shares one waker, so a single reply re-polls the whole
        // chunk. That is O(n²) polls in the worst case and it does not matter:
        // n is tens, a poll of an already-sent D-Bus call is a cheap look at a
        // reply slot, and the alternative is a scheduler.
        for (slot, fut) in done.iter_mut().zip(pending.iter_mut()) {
            if slot.is_none() {
                if let Poll::Ready(v) = fut.as_mut().poll(cx) {
                    *slot = Some(v);
                    finished += 1;
                }
            }
        }
        if finished == n { Poll::Ready(()) } else { Poll::Pending }
    })
    .await;

    // Every slot is `Some`: the poll loop does not return until all `n` are.
    done.into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A future that is not ready the first time it is polled, so the test
    /// exercises the re-poll path rather than a straight-line completion.
    struct Later(u32, usize);
    impl Future for Later {
        type Output = usize;
        fn poll(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<usize> {
            if self.0 == 0 {
                Poll::Ready(self.1)
            } else {
                self.0 -= 1;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    #[test]
    fn results_keep_their_order_however_they_finish() {
        // Deliberately reversed readiness: the last future completes first.
        let futs: Vec<Later> = (0..10usize).map(|i| Later(10 - i as u32, i)).collect();
        let out = futures_lite::future::block_on(join_all(futs));
        assert_eq!(out, (0..10).collect::<Vec<usize>>());
    }

    #[test]
    fn more_than_one_chunk_still_comes_back_in_order() {
        let n = MAX_IN_FLIGHT * 2 + 7;
        let futs: Vec<Later> = (0..n).map(|i| Later((i % 3) as u32, i)).collect();
        let out = futures_lite::future::block_on(join_all(futs));
        assert_eq!(out, (0..n).collect::<Vec<usize>>());
    }

    #[test]
    fn nothing_in_nothing_out() {
        let futs: Vec<Later> = Vec::new();
        assert!(futures_lite::future::block_on(join_all(futs)).is_empty());
    }
}
