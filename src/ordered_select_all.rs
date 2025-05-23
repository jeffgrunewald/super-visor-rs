use core::mem;
use core::pin::Pin;
use futures::{
    future::FutureExt,
    task::{Context, Poll},
    Future,
};

// This is a copy of the select_all function from futures::future::select_all. The only
// difference is the change from `swap_remove` to `remove` to preserve the order of
// the inner Vec.

#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` them"]
pub struct OrderedSelectAll<Fut> {
    inner: Vec<Fut>,
}

pub fn ordered_select_all<I>(iter: I) -> OrderedSelectAll<I::Item>
where
    I: IntoIterator,
    I::Item: Future + Unpin,
{
    OrderedSelectAll {
        inner: iter.into_iter().collect(),
    }
}

impl<Fut> OrderedSelectAll<Fut> {
    /// Consumes this combinator, returning the underlying futures.
    pub fn into_inner(self) -> Vec<Fut> {
        self.inner
    }
}

impl<Fut: Future + Unpin> Future for OrderedSelectAll<Fut> {
    type Output = (Fut::Output, usize, Vec<Fut>);

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
        let item = self
            .inner
            .iter_mut()
            .enumerate()
            .find_map(|(i, f)| match f.poll_unpin(ctx) {
                Poll::Pending => None,
                Poll::Ready(e) => Some((i, e)),
            });
        match item {
            Some((idx, res)) => {
                #[allow(clippy::let_underscore_future)]
                let _ = self.inner.remove(idx);
                let rest = mem::take(&mut self.inner);
                Poll::Ready((res, idx, rest))
            }
            None => Poll::Pending,
        }
    }
}
