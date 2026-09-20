//! Hue certificates name the bridge ID, not its IP. Trust on first pairing,
//! then pin the exact certificate (`couch_sdk::tls::Pin`, shared with the TV
//! clients). What is local to Hue is the ureq transport underneath it.
use couch_sdk::tls::Pin;
use rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned,
};
use std::{
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

/// What a person is shown when a bridge presents a certificate that is not
/// the one Couch pinned while they were standing at it.
pub const CHANGED: &str = "Hue bridge certificate changed; pair again";

/// The pinned verifier, and a flag saying it refused one.
///
/// rustls reports a refused certificate as an alert, which ureq hands back as
/// an ordinary I/O error - indistinguishable from a cable being pulled. A
/// pairing conversation has to tell those two apart, because one of them is a
/// bridge being slow and the other is something standing in front of it.
#[derive(Debug)]
struct Noted {
    pin: Pin,
    refused: Arc<AtomicBool>,
}

impl ServerCertVerifier for Noted {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let answer = self
            .pin
            .verify_server_cert(end, intermediates, name, ocsp, now);
        if answer.is_err() {
            self.refused.store(true, Ordering::SeqCst);
        }
        answer
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        couch_sdk::tls::verify_tls12_signature(message, cert, signed)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        couch_sdk::tls::verify_tls13_signature(message, cert, signed)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        couch_sdk::tls::supported_verify_schemes()
    }
}
use ureq::unversioned::{
    resolver::DefaultResolver,
    transport::{
        Buffers, ConnectionDetails, Connector, LazyBuffers, NextTimeout, TcpConnector, Transport,
        TransportAdapter,
    },
};
#[derive(Debug)]
struct PinnedConnector(Arc<ClientConfig>, bool);
impl<In: Transport> Connector<In> for PinnedConnector {
    type Out = PinnedTransport;
    fn connect(
        &self,
        d: &ConnectionDetails,
        input: Option<In>,
    ) -> Result<Option<Self::Out>, ureq::Error> {
        let input = input.ok_or(ureq::Error::Tls("Missing TCP connection"))?;
        let name = ServerName::try_from(
            d.uri
                .host()
                .ok_or(ureq::Error::Tls("Missing host"))?
                .to_string(),
        )
        .map_err(|_| ureq::Error::Tls("Invalid host"))?;
        let mut conn = ClientConnection::new(self.0.clone(), name)?;
        let mut sock = TransportAdapter::new(input.boxed());
        sock.set_timeout(d.timeout);
        conn.complete_io(&mut sock)?;
        Ok(Some(PinnedTransport {
            stream: StreamOwned::new(conn, sock),
            buffers: LazyBuffers::new(8192, 8192),
            stream_timeout: self.1,
        }))
    }
}
struct PinnedTransport {
    stream: StreamOwned<ClientConnection, TransportAdapter>,
    buffers: LazyBuffers,
    stream_timeout: bool,
}
impl Transport for PinnedTransport {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }
    fn transmit_output(&mut self, n: usize, t: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.sock.set_timeout(t);
        self.stream.write_all(&self.buffers.output()[..n])?;
        Ok(())
    }
    fn await_input(&mut self, mut t: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.sock.set_timeout(t);
        // Bound each idle read, not the lifetime of an active SSE response.
        if self.stream_timeout {
            t.after = t.after.min(std::time::Duration::from_secs(45).into());
            self.stream.sock.set_timeout(t);
        }
        let n = self.stream.read(self.buffers.input_append_buf())?;
        self.buffers.input_appended(n);
        Ok(n > 0)
    }
    fn is_open(&mut self) -> bool {
        self.stream.sock.get_mut().is_open()
    }
    fn is_tls(&self) -> bool {
        true
    }
}
/// Reads and the command line: five seconds, the deadline this client has
/// always had.
pub fn agent(certificate: Arc<Mutex<Vec<u8>>>) -> ureq::Agent {
    make_agent(certificate, false, 5, 5, Some(5))
}
/// Writes, which a person is waiting for with a finger on a slider: two
/// seconds to connect, three for the answer, four altogether. A write that
/// cannot be done in four seconds is better reported than waited for.
pub fn write_agent(certificate: Arc<Mutex<Vec<u8>>>) -> ureq::Agent {
    make_agent(certificate, false, 2, 3, Some(4))
}
/// Pairing, which is one request per poll of a dialog somebody is watching:
/// four seconds in total, the budget one step of a conversation may take. It
/// is deliberately longer in the answer than a write: a bridge issuing a key
/// is doing more work than a bridge switching a lamp, and a step that gives up
/// early costs the person a whole poll interval.
pub fn pair_agent(certificate: Arc<Mutex<Vec<u8>>>, refused: Arc<AtomicBool>) -> ureq::Agent {
    built(
        Arc::new(Noted {
            pin: Pin::new(certificate, CHANGED),
            refused,
        }),
        false,
        2,
        4,
        Some(4),
    )
}
/// The event stream, which has no deadline of its own: each idle read is
/// bounded instead (see `await_input`).
pub fn stream_agent(certificate: Arc<Mutex<Vec<u8>>>) -> ureq::Agent {
    make_agent(certificate, true, 5, 5, None)
}
fn make_agent(
    certificate: Arc<Mutex<Vec<u8>>>,
    stream: bool,
    connect: u64,
    response: u64,
    global: Option<u64>,
) -> ureq::Agent {
    built(
        Arc::new(Pin::new(certificate, CHANGED)),
        stream,
        connect,
        response,
        global,
    )
}

fn built(
    verifier: Arc<dyn ServerCertVerifier>,
    stream: bool,
    connect: u64,
    response: u64,
    global: Option<u64>,
) -> ureq::Agent {
    let tls = couch_sdk::tls::pinned_client_config(verifier).expect("TLS versions");
    let cfg = ureq::Agent::config_builder()
        .timeout_global(global.map(std::time::Duration::from_secs))
        .timeout_connect(Some(std::time::Duration::from_secs(connect)))
        .timeout_recv_response(Some(std::time::Duration::from_secs(response)))
        .max_redirects(0)
        .proxy(None)
        .build();
    ureq::Agent::with_parts(
        cfg,
        TcpConnector::default().chain(PinnedConnector(Arc::new(tls), stream)),
        DefaultResolver::default(),
    )
}
impl std::fmt::Debug for PinnedTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedTransport").finish_non_exhaustive()
    }
}
