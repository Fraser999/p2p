//! Port mapping context utilities.

use ECHO_REQ;

use futures::future::Loop;
use priv_prelude::*;
use protocol::Protocol;
use rust_sodium::crypto::box_::{PublicKey, gen_keypair};
use server_set::{ServerSet, Servers};
use tokio_io::codec::length_delimited::{self, Framed};

/// `P2p` allows you to manage how NAT traversal works.
///
/// You can edit rendezvous (traversal) servers, enable/Disable IGD use, etc.
#[derive(Default, Clone)]
pub struct P2p {
    inner: Arc<Mutex<P2pInner>>,
}

#[derive(Default, Clone)]
struct P2pInner {
    tcp_server_set: ServerSet,
    udp_server_set: ServerSet,
    igd_disabled: bool,
    igd_disabled_for_rendezvous: bool,
    force_use_local_port: bool,
}

// Some macros to reduce boilerplate

macro_rules! inner_get {
    ($self:ident, $field:ident) => {
        {
            let inner = unwrap!($self.inner.lock());
            inner.$field
        }
    };
}

macro_rules! inner_set {
    ($self:ident, $field:ident, $value:ident) => {
        {
            let mut inner = unwrap!($self.inner.lock());
            inner.$field = $value;
        }
    };
}

impl P2p {
    /// Check if IGD for rendezvous connections option is on or off.
    pub fn is_igd_enabled_for_rendezvous(&self) -> bool {
        !inner_get!(self, igd_disabled_for_rendezvous)
    }

    /// Try to use IGD port mapping when doing rendezvous connections.
    pub fn enable_igd_for_rendezvous(&self) {
        inner_set!(self, igd_disabled_for_rendezvous, false);
    }

    /// Don't use IGD port mapping when doing rendezvous connections.
    pub fn disable_igd_for_rendezvous(&self) {
        inner_set!(self, igd_disabled_for_rendezvous, true);
    }

    /// Tests if IGD use is enabled or not.
    /// It's enabled by default.
    pub fn is_igd_enabled(&self) -> bool {
        !inner_get!(self, igd_disabled)
    }

    /// Returns the value of `force_use_local_port` option.
    pub fn force_use_local_port(&self) -> bool {
        inner_get!(self, force_use_local_port)
    }

    /// If this option is on, when public address is determined, use our local listening port
    /// as external as well.
    pub fn set_force_use_local_port(&self, force: bool) {
        inner_set!(self, force_use_local_port, force);
    }

    /// By default `p2p` attempts to use IGD to open external ports for it's own use.
    /// Use this function to disable such behaviour.
    pub fn disable_igd(&self) {
        inner_set!(self, igd_disabled, true);
    }

    /// Re-enables IGD use.
    pub fn enable_igd(&self) {
        inner_set!(self, igd_disabled, false);
    }

    /// Tell about a `TcpTraversalServer` than can be used to help use perform rendezvous
    /// connects and hole punching.
    pub fn add_tcp_traversal_server(&self, addr: &PeerInfo) {
        self.add_server(Protocol::Tcp, addr);
    }

    /// Tells the library to forget a `TcpTraversalServer` previously added with
    /// `add_tcp_traversal_server`.
    pub fn remove_tcp_traversal_server(&self, addr: SocketAddr) {
        self.remove_server(Protocol::Tcp, addr);
    }

    /// Returns a iterator over all tcp traversal server addresses.
    pub fn tcp_traversal_servers(&self) -> Servers {
        self.iter_servers(Protocol::Tcp)
    }

    /// Tell about a `UdpTraversalServer` than can be used to help use perform rendezvous
    /// connects and hole punching.
    pub fn add_udp_traversal_server(&self, addr: &PeerInfo) {
        self.add_server(Protocol::Udp, addr);
    }

    /// Tells the library to forget a `UdpTraversalServer` previously added with
    /// `add_udp_traversal_server`.
    pub fn remove_udp_traversal_server(&self, addr: SocketAddr) {
        self.remove_server(Protocol::Udp, addr);
    }

    /// Returns an iterator over all udp traversal server addresses added with
    /// `add_tcp_traversal_server`.
    pub fn udp_traversal_servers(&self) -> Servers {
        self.iter_servers(Protocol::Udp)
    }

    /// Returns a `Stream` of traversal servers.
    pub fn iter_servers(&self, protocol: Protocol) -> Servers {
        let mut inner = unwrap!(self.inner.lock());
        inner.server_set(protocol).iter_servers()
    }

    fn add_server(&self, protocol: Protocol, addr: &PeerInfo) {
        let mut inner = unwrap!(self.inner.lock());
        inner.server_set(protocol).add_server(addr);
    }

    fn remove_server(&self, protocol: Protocol, addr: SocketAddr) {
        let mut inner = unwrap!(self.inner.lock());
        inner.server_set(protocol).remove_server(addr);
    }
}

impl P2pInner {
    fn server_set(&mut self, protocol: Protocol) -> &mut ServerSet {
        match protocol {
            Protocol::Udp => &mut self.udp_server_set,
            Protocol::Tcp => &mut self.tcp_server_set,
        }
    }
}

/// Request that has sender's public key and arbitrary body.
/// This request is SHOULD be anonymously encrypted and it allows receiver to switch to
/// authenticated encryption.
#[derive(Serialize, Deserialize)]
pub struct EncryptedRequest {
    /// Sender's public key. Response should be encrypted with it.
    pub our_pk: PublicKey,
    /// Arbitrary request body.
    pub body: Vec<u8>,
}

impl EncryptedRequest {
    /// Create new request.
    pub fn new(our_pk: PublicKey, body: Vec<u8>) -> Self {
        Self { our_pk, body }
    }
}

/// Sends request to echo address server and returns our public address on success.
pub fn query_public_addr(
    protocol: Protocol,
    bind_addr: &SocketAddr,
    server_info: &PeerInfo,
    handle: &Handle,
) -> BoxFuture<SocketAddr, QueryPublicAddrError> {
    let (our_pk, our_sk) = gen_keypair(); // TODO(povilas): pass this from upper layers. MAID-2532
    let request = EncryptedRequest::new(our_pk, ECHO_REQ.to_vec());
    // First message is encrypted anonymously.
    let crypto_ctx = CryptoContext::anonymous_encrypt(server_info.pub_key);
    let encrypted_req = match crypto_ctx.encrypt(&request) {
        Ok(data) => data,
        Err(e) => return future::err(QueryPublicAddrError::Encrypt(e)).into_boxed(),
    };

    // Response comes encrypted and authenticated.
    let crypto_ctx = CryptoContext::authenticated(server_info.pub_key, our_sk);
    match protocol {
        Protocol::Tcp => {
            tcp_query_public_addr(bind_addr, server_info, handle, crypto_ctx, encrypted_req)
        }
        Protocol::Udp => {
            udp_query_public_addr(bind_addr, server_info, handle, crypto_ctx, encrypted_req)
        }
    }
}

quick_error! {
    /// Error indicating failure to retrieve our public address.
    #[derive(Debug)]
    pub enum QueryPublicAddrError {
        /// Failed to bind to socket before even starting a query.
        Bind(e: io::Error) {
            description("error binding to socket address")
            display("error binding to socket address: {}", e)
            cause(e)
        }
        /// Connection failure.
        Connect(e: io::Error) {
            description("error connecting to echo server")
            display("error connecting to echo server: {}", e)
            cause(e)
        }
        /// Query timed out.
        ConnectTimeout {
            description("timed out contacting server")
        }
        /// Error sending query.
        SendRequest(e: io::Error) {
            description("error sending request to echo server")
            display("error sending request to echo server: {}", e)
            cause(e)
        }
        /// Error receiving query.
        ReadResponse(e: io::Error) {
            description("error reading response from echo server")
            display("error reading response from echo server: {}", e)
            cause(e)
        }
        /// Respone timed out.
        ResponseTimeout {
            description("timed out waiting for response from echo server")
        }
        /// Failure to encrypt request.
        Encrypt(e: CryptoError) {
            description("Error encrypting message")
            display("Error encrypting message: {}", e)
            cause(e)
        }
        /// Failure to decrypt request.
        Decrypt(e: CryptoError) {
            description("Error decrypting message")
            display("Error decrypting message: {}", e)
            cause(e)
        }
    }
}

/// Queries our public IP.
pub fn tcp_query_public_addr(
    bind_addr: &SocketAddr,
    server_info: &PeerInfo,
    handle: &Handle,
    crypto_ctx: CryptoContext,
    encrypted_req: BytesMut,
) -> BoxFuture<SocketAddr, QueryPublicAddrError> {
    let bind_addr = *bind_addr;
    let server_addr = server_info.addr;
    let handle = handle.clone();

    TcpStream::connect_reusable(&bind_addr, &server_addr, &handle)
        // TODO(povilas): use QueryPublicAddrError::from(ConnectReusableError)
        .map_err(|err| match err {
            ConnectReusableError::Connect(e) => QueryPublicAddrError::Connect(e),
            ConnectReusableError::Bind(e) => QueryPublicAddrError::Bind(e),
        })
        .with_timeout(Duration::from_secs(3), &handle)
        .and_then(|opt| opt.ok_or(QueryPublicAddrError::ConnectTimeout))
        .map(|stream| length_delimited::Builder::new().new_framed(stream))
        .and_then(move |stream| {
            stream.send(encrypted_req).map_err(
                QueryPublicAddrError::SendRequest,
            )
        })
        .and_then(move |stream| tcp_recv_echo_addr(&handle, stream, crypto_ctx))
        .into_boxed()
}

fn tcp_recv_echo_addr(
    handle: &Handle,
    stream: Framed<TcpStream>,
    crypto_ctx: CryptoContext,
) -> BoxFuture<SocketAddr, QueryPublicAddrError> {
    stream
        .into_future()
        .map_err(|(err, _stream)| QueryPublicAddrError::ReadResponse(err))
        .and_then(|(resp_opt, _stream)| {
            resp_opt.ok_or_else(|| {
                QueryPublicAddrError::ReadResponse(io::ErrorKind::ConnectionReset.into())
            })
        })
        .and_then(move |resp| {
            crypto_ctx.decrypt(&resp).map_err(
                QueryPublicAddrError::Decrypt,
            )
        })
        .with_timeout(Duration::from_secs(2), handle)
        .and_then(|opt| opt.ok_or(QueryPublicAddrError::ResponseTimeout))
        .into_boxed()
}

pub fn udp_query_public_addr(
    bind_addr: &SocketAddr,
    server_info: &PeerInfo,
    handle: &Handle,
    crypto_ctx: CryptoContext,
    encrypted_req: BytesMut,
) -> BoxFuture<SocketAddr, QueryPublicAddrError> {
    let try = || {
        let bind_addr = *bind_addr;
        let server_addr = server_info.addr;
        let handle = handle.clone();
        let socket = {
            UdpSocket::bind_connect_reusable(&bind_addr, &server_addr, &handle)
                .map_err(QueryPublicAddrError::Bind)
        }?;

        Ok({
            socket
                .send_dgram(encrypted_req, server_addr)
                .map(|(socket, _buf)| socket)
                .map_err(QueryPublicAddrError::SendRequest)
                .and_then(move |socket| {
                    udp_recv_echo_addr(&handle, socket, server_addr, crypto_ctx)
                })
        })
    };
    future::result(try()).flatten().into_boxed()
}

fn udp_recv_echo_addr(
    handle: &Handle,
    socket: UdpSocket,
    server_addr: SocketAddr,
    crypto_ctx: CryptoContext,
) -> BoxFuture<SocketAddr, QueryPublicAddrError> {
    future::loop_fn(socket, move |socket| {
        let crypto_ctx = crypto_ctx.clone();
        socket
            .recv_dgram(vec![0u8; 256])
            .map_err(QueryPublicAddrError::ReadResponse)
            .and_then(move |(socket, data, len, addr)| if addr == server_addr {
                trace!("server responded with: {:?}", &data[..len]);
                let data = crypto_ctx.decrypt(&data[..len]).map_err(
                    QueryPublicAddrError::Decrypt,
                )?;
                Ok(Loop::Break(data))
            } else {
                Ok(Loop::Continue(socket))
            })
    }).with_timeout(Duration::from_secs(2), handle)
        .and_then(|opt| opt.ok_or(QueryPublicAddrError::ResponseTimeout))
        .into_boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    mod p2p {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn it_creates_mapping_context_with_igd_enabled() {
                let p2p = P2p::default();

                assert!(p2p.is_igd_enabled())
            }

            #[test]
            fn it_creates_mapping_context_with_igd_enabled_for_rendezvous() {
                let p2p = P2p::default();

                assert!(p2p.is_igd_enabled_for_rendezvous())
            }

            #[test]
            fn it_creates_mapping_context_with_force_use_local_port_disabled() {
                let p2p = P2p::default();

                assert!(!p2p.force_use_local_port())
            }
        }

        mod tcp_traversal_servers {
            use super::*;

            #[test]
            fn it_returns_current_tcp_traversal_servers() {
                let p2p = P2p::default();

                p2p.add_tcp_traversal_server(&peer_addr!("1.2.3.4:4000"));
                p2p.add_tcp_traversal_server(&peer_addr!("1.2.3.5:5000"));

                let addrs = p2p.tcp_traversal_servers().addrs_snapshot();
                assert!(addrs.contains(&addr!("1.2.3.4:4000")));
                assert!(addrs.contains(&addr!("1.2.3.5:5000")));
            }
        }

        mod remove_tcp_traversal_server {
            use super::*;

            #[test]
            fn it_removes_given_server_from_the_list_if_it_exists() {
                let p2p = P2p::default();
                p2p.add_tcp_traversal_server(&peer_addr!("1.2.3.4:4000"));
                p2p.add_tcp_traversal_server(&peer_addr!("1.2.3.5:5000"));

                p2p.remove_tcp_traversal_server(addr!("1.2.3.4:4000"));

                let addrs = p2p.tcp_traversal_servers().addrs_snapshot();
                assert!(addrs.contains(&addr!("1.2.3.5:5000")));
                assert!(!addrs.contains(&addr!("1.2.3.4:4000")));
            }

            #[test]
            fn it_does_nothing_if_give_address_is_not_in_the_list() {
                let p2p = P2p::default();
                p2p.add_tcp_traversal_server(&peer_addr!("1.2.3.5:5000"));

                p2p.remove_tcp_traversal_server(addr!("1.2.3.4:4000"));

                let addrs = p2p.tcp_traversal_servers().addrs_snapshot();
                assert!(addrs.contains(&addr!("1.2.3.5:5000")));
            }
        }
    }
}
