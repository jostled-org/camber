//! A protocol refusal cannot start a retry outside Camber's coordinator.
//!
//! HTTP/2 is enabled by the dev dependency to exercise feature unification.
//! The same peer makes an ordinary Reqwest client retry, calibrating the fault.
//! Removing `retry::never()` from Camber's factory must fail every Camber row.

use crate::tls_support::{build_server_config, generate_cert_with_san};
use camber::RuntimeError;
use camber::http::{ClientError, EgressPolicy, client};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

const BOUND: Duration = Duration::from_secs(10);

struct Peer {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    certificate: Box<[u8]>,
    url: Box<str>,
}

impl Peer {
    async fn new() -> Self {
        let (certificate, key) = generate_cert_with_san("127.0.0.1");
        let mut config = build_server_config(&certificate, &key);
        Arc::get_mut(&mut config).unwrap().alpn_protocols = vec![b"h2".to_vec()];
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        Self {
            listener,
            acceptor: TlsAcceptor::from(config),
            certificate: certificate.into_boxed_slice(),
            url: url.into_boxed_str(),
        }
    }

    async fn serve(&self, mut stopped: oneshot::Receiver<()>) -> usize {
        let (socket, _) = self.listener.accept().await.unwrap();
        let tls = self.acceptor.accept(socket).await.unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(b"h2".as_slice()));
        let mut connection = h2::server::handshake(tls).await.unwrap();
        let mut requests = 0;
        loop {
            let received = tokio::select! {
                _ = &mut stopped => return requests,
                request = connection.accept() => request,
            };
            let request = match received {
                Some(request) => request,
                None => return requests,
            };
            let (request, mut answer) = request.unwrap();
            assert_eq!(request.method(), hyper::Method::GET);
            requests += 1;
            match requests {
                1 => answer.send_reset(h2::Reason::REFUSED_STREAM),
                _ => {
                    answer
                        .send_response(hyper::Response::new(()), true)
                        .unwrap();
                }
            }
        }
    }
}

async fn calibrate_protocol_refusal() {
    let peer = Peer::new().await;
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_certs_merge([reqwest::Certificate::from_pem(&peer.certificate).unwrap()])
        .build()
        .unwrap();
    let (stop, stopped) = oneshot::channel();
    let call = async {
        let result = client.get(peer.url.as_ref()).send().await;
        let _ = stop.send(());
        result
    };
    let (result, requests) = tokio::join!(call, peer.serve(stopped));
    assert_eq!(result.unwrap().status(), 200);
    assert_eq!(requests, 2, "the peer must trigger Reqwest's default retry");
}

async fn camber_refuses_hidden_retry(restricted: bool, streaming: bool) {
    let peer = Peer::new().await;
    let client = client()
        .retries(0)
        .add_root_certificate(&peer.certificate)
        .unwrap();
    let client = match restricted {
        true => client.egress_policy(
            EgressPolicy::public_internet()
                .allow_destination(&peer.url, IpAddr::V4(Ipv4Addr::LOCALHOST))
                .unwrap(),
        ),
        false => client,
    };
    let (stop, stopped) = oneshot::channel();
    let call = async {
        let request = client.request("GET", &peer.url).unwrap();
        let result = match streaming {
            true => request.send_streaming().await.map(|_| ()),
            false => request.send().await.map(|_| ()),
        };
        let _ = stop.send(());
        result
    };
    let (result, requests) = tokio::join!(call, peer.serve(stopped));
    assert_eq!(
        requests, 1,
        "restricted={restricted}, streaming={streaming}: hidden retry"
    );
    assert!(matches!(
        result,
        Err(RuntimeError::Client(ClientError::Transport))
    ));
}

#[tokio::test]
async fn protocol_refusal_cannot_bypass_the_retry_budget() {
    tokio::time::timeout(BOUND, async {
        calibrate_protocol_refusal().await;
        for (restricted, streaming) in [(false, false), (false, true), (true, false), (true, true)]
        {
            camber_refuses_hidden_retry(restricted, streaming).await;
        }
    })
    .await
    .expect("protocol retry proof exceeded its watchdog");
}
