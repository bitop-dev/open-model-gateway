mod dbtest_runtime {
    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    type TestBody = fn(u8) -> Pin<Box<dyn Future<Output = ()>>>;

    pub fn run(path: &str, body: TestBody) {
        assert_eq!(path, "name_collision::pool");
        let mut future = body(42);
        assert_eq!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
    }
}

#[dbtest::test]
async fn pool(value: u8) {
    assert_eq!(value, 42);
}
