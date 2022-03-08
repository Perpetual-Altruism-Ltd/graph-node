use std::collections::VecDeque;
use std::fmt::Display;
use std::sync::Arc;

use futures::stream;
use futures::stream::StreamExt;
use graph::cheap_clone::CheapClone;
use graph::parking_lot::Mutex;
use graph::prelude::tokio;
use graph::slog::{debug, Logger};
use tokio::sync::{mpsc, watch};
use tower::{Service, ServiceExt};

/// Spawn a monitor that actively polls a service. Whenever the service has capacity, the monitor
/// pulls object ids from the queue and polls the service. If the object is not present or in case
/// of error, the object id is pushed to the back of the queue to be polled again.
///
/// The service returns the request ID along with errors or responses. The response is an
/// `Option`, to represent the object not being found. `S::poll_ready` should never error.
pub fn spawn_monitor<ID, S, E, Response: Send + 'static>(
    service: S,
    response_sender: mpsc::Sender<(ID, Response)>,
    logger: Logger,
) -> PollingMonitor<ID>
where
    ID: Display + Send + 'static,
    S: Service<ID, Response = (ID, Option<Response>), Error = (ID, E)> + Send + 'static,
    E: Display + Send + 'static,
    S::Future: Send,
{
    let queue = Arc::new(Mutex::new(VecDeque::new()));
    let (wake_up_queue, queue_woken) = watch::channel(());

    let queue_to_stream = {
        let queue = queue.cheap_clone();
        stream::unfold((), move |()| {
            let queue = queue.cheap_clone();
            let mut queue_woken = queue_woken.clone();
            async move {
                loop {
                    let id = queue.lock().pop_front();
                    match id {
                        Some(id) => break Some((id, ())),
                        None => match queue_woken.changed().await {
                            // Queue woken, check it.
                            Ok(()) => {}

                            // The `PollingMonitor` has been dropped, cancel this task.
                            Err(_) => break None,
                        },
                    };
                }
            }
        })
    };

    {
        let queue = queue.cheap_clone();
        graph::spawn(async move {
            let mut responses = service.call_all(queue_to_stream).boxed();
            while let Some(response) = responses.next().await {
                match response {
                    Ok((id, Some(response))) => {
                        let send_result = response_sender.send((id, response)).await;
                        if send_result.is_err() {
                            // The receiver has been dropped, cancel this task.
                            break;
                        }
                    }

                    // Object not found, push the id to the back of the queue.
                    Ok((id, None)) => queue.lock().push_back(id),

                    // Error polling, log it and push the id to the back of the queue.
                    Err((id, e)) => {
                        debug!(logger, "error polling";
                                    "error" => format!("{:#}", e),
                                    "object_id" => id.to_string());
                        queue.lock().push_back(id)
                    }
                }
            }
        });
    }

    PollingMonitor {
        queue,
        wake_up_queue,
    }
}

/// Handle for adding objects to be monitored.
pub struct PollingMonitor<ID> {
    queue: Arc<Mutex<VecDeque<ID>>>,

    // This serves two purposes, to wake up the monitor when an item arrives on an empty queue, and
    // to stop the montior task when this handle is dropped.
    wake_up_queue: watch::Sender<()>,
}

impl<ID> PollingMonitor<ID> {
    /// Add an object id to the polling queue. New requests have priority and are pushed to the
    /// front of the queue.
    pub fn monitor(&self, id: ID) {
        let mut queue = self.queue.lock();
        if queue.is_empty() {
            // If the send fails, the response receiver has been dropped, so this handle is useless.
            let _ = self.wake_up_queue.send(());
        }
        queue.push_front(id);
    }
}

//#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, pin::Pin, task::Poll};

    use futures::{Future, FutureExt, TryFutureExt};

    use graph::log;
    use tower_test::mock;

    use super::*;

    // #[derive(Default)]
    // struct MockService {
    //     count: BTreeMap<&'static str, u32>,
    //     responses: BTreeMap<
    //         &'static str,
    //         // This is `VecDeque<Result<Self::Response, Self::Error>>`
    //         VecDeque<<<Self as Service<&'static str>>::Future as Future>::Output>,
    //     >,
    // }

    // impl Service<&'static str> for PollCounter {
    //     type Response = Option<()>;

    //     type Error = ();

    //     type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>>>>;

    //     fn poll_ready(&mut self, _: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
    //         Poll::Ready(Ok(()))
    //     }

    //     fn call(&mut self, req: &'static str) -> Self::Future {
    //         *self.count.entry(req).or_default() += 1;

    //         let res = self
    //             .responses
    //             .get_mut(req)
    //             .and_then(|res| res.pop_front())
    //             .transpose()
    //             .map(|opt| opt.flatten());

    //         async move { res }.boxed()
    //     }
    // }

    struct MockService(mock::Mock<&'static str, Option<&'static str>>);

    impl Service<&'static str> for MockService {
        type Response = (&'static str, Option<&'static str>);

        type Error = (&'static str, Box<dyn std::error::Error + Send + Sync>);

        type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

        fn poll_ready(&mut self, cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
            self.0.poll_ready(cx).map_err(|_| unreachable!())
        }

        fn call(&mut self, req: &'static str) -> Self::Future {
            self.0
                .call(req)
                .map_ok(move |x| (req, x))
                .map_err(move |e| (req, e))
                .boxed()
        }
    }

    #[tokio::test]
    async fn test_polling_monitor() {
        let (service, mut handle) = mock::pair();
        let service = MockService(service);
        let (tx, rx) = mpsc::channel(10);
        let monitor = spawn_monitor(service, tx, log::discard());
        // assert!(!service());
    }
}
