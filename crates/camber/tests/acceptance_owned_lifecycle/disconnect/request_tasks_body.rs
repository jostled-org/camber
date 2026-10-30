//! User body polling retains runtime context after the response head is sent.

use super::fixture::{BOUND, bounded};
use super::routes::probe_router;
use super::servers::with_owned_server;
use crate::common::PersistentH2Client;
use camber::http::GrpcRouter;
use std::convert::Infallible;
use std::future::{Future, Ready, ready};
use std::pin::Pin;
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll};

type TaskPair = (camber::JoinHandle<usize>, camber::AsyncJoinHandle<usize>);

#[derive(Clone)]
struct BodyService {
    release: Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>,
    report: mpsc::Sender<TaskPair>,
}

impl tonic::server::NamedService for BodyService {
    const NAME: &'static str = "probe.Tasks";
}

impl<B> tonic::codegen::Service<hyper::Request<B>> for BodyService {
    type Response = hyper::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: hyper::Request<B>) -> Self::Future {
        let body = BodyTasks {
            release: self.release.lock().unwrap().take().unwrap(),
            report: Some(self.report.clone()),
        };
        ready(Ok(hyper::Response::new(tonic::body::Body::new(body))))
    }
}

struct BodyTasks {
    release: tokio::sync::oneshot::Receiver<()>,
    report: Option<mpsc::Sender<TaskPair>>,
}

impl hyper::body::Body for BodyTasks {
    type Data = bytes::Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        if self.report.is_none() {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.release).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(released) => {
                released.unwrap();
                self.report
                    .take()
                    .unwrap()
                    .send((camber::spawn(|| 17), camber::spawn_async(async { 18 })))
                    .unwrap();
                Poll::Ready(None)
            }
        }
    }
}

#[test]
fn request_tasks_native_grpc_body_spawns_after_head_and_suspension() {
    let (release, wait) = tokio::sync::oneshot::channel();
    let (report, observed) = mpsc::channel();
    let service = BodyService {
        release: Arc::new(Mutex::new(Some(wait))),
        report,
    };
    let mut router = probe_router();
    router.grpc(GrpcRouter::new().add_service(service));
    with_owned_server(router, |addr| {
        bounded("gRPC body tasks", async {
            let mut client = PersistentH2Client::connect(addr, BOUND).await;
            let mut stream = client
                .open_paced(
                    "POST",
                    "/probe.Tasks/Run",
                    "localhost",
                    &[("content-type", "application/grpc")],
                )
                .await;
            stream.finish();
            let response = stream.take_response().await.unwrap();
            assert_eq!(response.status(), 200);
            release.send(()).unwrap();
            let mut body = response.into_body();
            while let Some(frame) = body.data().await {
                assert!(frame.unwrap().is_empty());
            }
            let (blocking, asynchronous) = observed.recv_timeout(BOUND).unwrap();
            assert_eq!(blocking.await.unwrap(), 17);
            assert_eq!(asynchronous.await.unwrap(), 18);
            drop(body);
            drop(stream);
            client.close().await;
        })
    });
}
