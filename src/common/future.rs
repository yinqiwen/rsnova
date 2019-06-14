use futures::{Future, Poll};
/// Combines two different futures yielding the same item and error
/// types into a single type.
#[derive(Debug)]
pub enum FourEither<A, B, C, D> {
    /// First branch of the type
    A(A),
    /// Second branch of the type
    B(B),
    C(C),
    D(D),
}

impl<T, A, B, C, D> FourEither<(T, A), (T, B), (T, C), (T, D)> {
    /// Splits out the homogeneous type from an either of tuples.
    ///
    /// This method is typically useful when combined with the `Future::select2`
    /// combinator.
    pub fn split(self) -> (T, FourEither<A, B, C, D>) {
        match self {
            FourEither::A((a, b)) => (a, FourEither::A(b)),
            FourEither::B((a, b)) => (a, FourEither::B(b)),
            FourEither::C((a, b)) => (a, FourEither::C(b)),
            FourEither::D((a, b)) => (a, FourEither::D(b)),
        }
    }
}

impl<A, B, C, D> Future for FourEither<A, B, C, D>
where
    A: Future,
    B: Future<Item = A::Item, Error = A::Error>,
    C: Future<Item = A::Item, Error = A::Error>,
    D: Future<Item = A::Item, Error = A::Error>,
{
    type Item = A::Item;
    type Error = A::Error;

    fn poll(&mut self) -> Poll<A::Item, A::Error> {
        match *self {
            FourEither::A(ref mut a) => a.poll(),
            FourEither::B(ref mut b) => b.poll(),
            FourEither::C(ref mut c) => c.poll(),
            FourEither::D(ref mut d) => d.poll(),
        }
    }
}
