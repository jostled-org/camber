/// Streaming body backed by an mpsc channel.
pub(super) struct StreamBody {
    pub(super) rx: tokio::sync::mpsc::Receiver<bytes::Bytes>,
}

impl hyper::body::Body for StreamBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.rx.poll_recv(cx) {
            std::task::Poll::Ready(Some(data)) => {
                std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(data))))
            }
            std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

pub(super) enum HyperResponseBody {
    Full(http_body_util::Full<bytes::Bytes>),
    Streaming(StreamBody),
    #[cfg(feature = "grpc")]
    Grpc(GrpcBody),
}

impl hyper::body::Body for HyperResponseBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            HyperResponseBody::Full(body) => std::pin::Pin::new(body).poll_frame(cx),
            HyperResponseBody::Streaming(body) => std::pin::Pin::new(body).poll_frame(cx),
            #[cfg(feature = "grpc")]
            HyperResponseBody::Grpc(body) => body.poll_frame(cx),
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        match self {
            HyperResponseBody::Full(body) => body.size_hint(),
            HyperResponseBody::Streaming(body) => body.size_hint(),
            #[cfg(feature = "grpc")]
            HyperResponseBody::Grpc(body) => body.size_hint(),
        }
    }
}

/// Body wrapper for gRPC responses (tonic's UnsyncBoxBody).
#[cfg(feature = "grpc")]
pub(super) struct GrpcBody {
    pub(super) inner: tonic::body::BoxBody,
    pub(super) finished: bool,
}

#[cfg(feature = "grpc")]
impl GrpcBody {
    pub(super) fn poll_frame(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<bytes::Bytes>, std::convert::Infallible>>>
    {
        use hyper::body::Body;
        if self.finished {
            return std::task::Poll::Ready(None);
        }

        match std::pin::Pin::new(&mut self.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => std::task::Poll::Ready(Some(Ok(frame))),
            std::task::Poll::Ready(Some(Err(status))) => {
                self.finished = true;
                std::task::Poll::Ready(Some(Ok(grpc_error_frame(&status))))
            }
            std::task::Poll::Ready(None) => {
                self.finished = true;
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    pub(super) fn size_hint(&self) -> hyper::body::SizeHint {
        use hyper::body::Body;
        self.inner.size_hint()
    }
}

#[cfg(feature = "grpc")]
fn grpc_error_frame(status: &tonic::Status) -> hyper::body::Frame<bytes::Bytes> {
    let mut trailers = hyper::HeaderMap::with_capacity(2);
    let code = status.code() as i32;
    trailers.insert("grpc-status", hyper::header::HeaderValue::from(code));
    if let Ok(message) = hyper::header::HeaderValue::from_str(status.message()) {
        trailers.insert("grpc-message", message);
    }
    hyper::body::Frame::trailers(trailers)
}
