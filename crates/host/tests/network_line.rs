//! edualcd's network line, end to end as far as the daemon's side of it: a client of the line
//! (TLS pinned to the certificate, a WebSocket, frames as the Unix socket carries them) against
//! `net::router` on `net::serve`, the lines it lets in coming out as the daemon's [`Line`]s.

use std::net::SocketAddr;
use std::time::Duration;

use edualc_host::daemon::Line;
use edualc_host::ipc::pairing::Fingerprint;
use edualc_host::ipc::{self, Auth, Deframer, FromDaemon, PROTOCOL, ToDaemon, Token};
use edualc_host::net::{self, Access};
use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, Message};

type Client = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

/// An edualcd's network side, listening.
struct Served {
    port: u16,
    fingerprint: Fingerprint,
    lines: mpsc::Receiver<Line>,
}

async fn served() -> Served {
    let made =
        rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).expect("a certificate");
    let certificate = made.cert.der().to_vec();
    let fingerprint = Fingerprint::of(&certificate);
    let acceptor =
        ipc::tls::acceptor(certificate, made.signing_key.serialize_der()).expect("TLS serves");
    let (lines, taken) = mpsc::channel(8);
    let app = net::router(Access::new(Token::new("secret".to_owned()), lines));
    let port = {
        let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        probe.local_addr().expect("its address").port()
    };
    let listen = SocketAddr::from(([127, 0, 0, 1], port));
    tokio::spawn(net::serve(listen, acceptor, app));
    // Listening before it is reached.
    for _ in 0..100 {
        if TcpStream::connect(listen).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Served {
        port,
        fingerprint,
        lines: taken,
    }
}

async fn connect(served: &Served, protocol: u32) -> Result<Client, tungstenite::Error> {
    let tcp = TcpStream::connect(("127.0.0.1", served.port))
        .await
        .expect("connects");
    let connector = ipc::tls::connector(served.fingerprint).expect("a connector");
    let secured = connector
        .connect(
            rustls::pki_types::ServerName::try_from("localhost").expect("a name"),
            tcp,
        )
        .await
        .expect("TLS, with the certificate pinned");
    let url = format!(
        "wss://localhost:{}/api/line?protocol={protocol}",
        served.port
    );
    tokio_tungstenite::client_async(url, secured)
        .await
        .map(|(client, _response)| client)
}

fn binary(message: &impl serde::Serialize) -> Message {
    Message::Binary(ipc::frame(message).expect("a frame").into())
}

fn auth(token: &str) -> Message {
    binary(&Auth {
        token: Token::new(token.to_owned()),
    })
}

/// What the client gets next that isn't a ping, in time.
async fn next(client: &mut Client) -> Option<Message> {
    loop {
        match tokio::time::timeout(Duration::from_secs(8), client.next()).await {
            Ok(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
            Ok(Some(Ok(message))) => return Some(message),
            Ok(Some(Err(_)) | None) | Err(_) => return None,
        }
    }
}

#[tokio::test]
async fn a_line_with_the_token_is_a_window_of_the_daemon() {
    let mut served = served().await;
    let mut client = connect(&served, PROTOCOL).await.expect("the line opens");
    client.send(auth("secret")).await.expect("sent");
    client
        .send(binary(&ToDaemon::Hello { protocol: PROTOCOL }))
        .await
        .expect("sent");
    let mut line = tokio::time::timeout(Duration::from_secs(5), served.lines.recv())
        .await
        .expect("in time")
        .expect("a line");
    assert!(matches!(
        line.requests.recv().await,
        Some(ToDaemon::Hello { protocol }) if protocol == PROTOCOL
    ));
    line.outbox
        .send(FromDaemon::Hosted(Vec::new()))
        .await
        .expect("sent");
    let Some(Message::Binary(frame)) = next(&mut client).await else {
        panic!("what the daemon says reaches the window");
    };
    let mut deframer = Deframer::<FromDaemon>::default();
    deframer.push(&frame).expect("a frame");
    assert!(matches!(deframer.take(), Some(FromDaemon::Hosted(hosted)) if hosted.is_empty()));
}

#[tokio::test]
async fn a_wrong_token_is_refused_and_no_line_is_made() {
    let mut served = served().await;
    let mut client = connect(&served, PROTOCOL).await.expect("the line opens");
    client.send(auth("secreT")).await.expect("sent");
    match next(&mut client).await {
        Some(Message::Close(Some(frame))) => assert_eq!(u16::from(frame.code), 4401),
        other => panic!("closed as refused, not {other:?}"),
    }
    assert!(served.lines.try_recv().is_err(), "no line for it");
}

#[tokio::test]
async fn what_isnt_a_token_in_a_frame_of_its_own_is_refused() {
    let mut served = served().await;
    // A message first, as if the line were let in already.
    let mut first = connect(&served, PROTOCOL).await.expect("the line opens");
    first
        .send(binary(&ToDaemon::Hello { protocol: PROTOCOL }))
        .await
        .expect("sent");
    assert!(matches!(
        next(&mut first).await,
        Some(Message::Close(_)) | None
    ));
    // A frame far larger than a token.
    let mut large = connect(&served, PROTOCOL).await.expect("the line opens");
    large
        .send(Message::Binary(vec![1u8; 4096].into()))
        .await
        .expect("sent");
    assert!(matches!(
        next(&mut large).await,
        Some(Message::Close(_)) | None
    ));
    // Text.
    let mut text = connect(&served, PROTOCOL).await.expect("the line opens");
    text.send(Message::Text("secret".into()))
        .await
        .expect("sent");
    assert!(matches!(
        next(&mut text).await,
        Some(Message::Close(_)) | None
    ));
    assert!(served.lines.try_recv().is_err(), "no line for any");
}

#[tokio::test]
async fn a_line_that_shows_no_token_in_time_is_closed() {
    let mut served = served().await;
    let mut silent = connect(&served, PROTOCOL).await.expect("the line opens");
    assert!(matches!(
        next(&mut silent).await,
        Some(Message::Close(_)) | None
    ));
    assert!(served.lines.try_recv().is_err(), "no line for it");
}

#[tokio::test]
async fn a_line_of_another_protocol_is_refused_before_it_opens() {
    let served = served().await;
    match connect(&served, PROTOCOL + 1).await {
        Err(tungstenite::Error::Http(response)) => assert_eq!(response.status(), 409),
        Ok(_) => panic!("opened"),
        Err(other) => panic!("refused as another version, not {other}"),
    }
}
