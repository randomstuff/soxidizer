mod socks;

use std::collections::HashSet;
use std::env;
use std::io::Error;
use std::io::ErrorKind;
use std::mem::size_of_val;
use std::net::AddrParseError;
use std::net::SocketAddr;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use libc::c_int;
use libc::c_void;
use libc::getsockopt;
use libc::socklen_t;
use libc::AF_INET;
use libc::AF_INET6;
use libc::SOL_SOCKET;
use libc::SO_DOMAIN;
use socks::AddressType;
use socks::REP_SUCCEEDED;
use socks::{
    read_client_hello, read_socks_request, COMMAND_CONNECT, NO_ACCEPTABLE_AUTHENTICATION,
    NO_AUTHENTICATION, SOCKS_VERSION5,
};
use tokio::fs::remove_file;
use tokio::io::copy_bidirectional;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::net::unix::uid_t;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::signal;
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::instrument;
use tracing::{debug, info};

use crate::socks::SocksRequestAddress;
use crate::socks::REP_ADDRESS_TYPE_NOT_SUPPORTED;
use crate::socks::{REP_COMMAND_NOT_SUPPORTED, REP_CONNECTION_NOT_ALLOWED, REP_HOST_NOT_REACHABLE};

trait GenericStream: AsyncRead + AsyncWrite {
    fn get_uid(&self) -> Result<uid_t, std::io::Error>;
}

impl GenericStream for UnixStream {
    fn get_uid(&self) -> Result<uid_t, std::io::Error> {
        self.peer_cred().map(|cred| cred.uid())
    }
}

impl GenericStream for TcpStream {
    fn get_uid(&self) -> Result<uid_t, std::io::Error> {
        // Not implemented yet:
        return Err(Error::from(ErrorKind::Other));
    }
}

#[derive(Parser, Debug)]
struct CliArguments {
    #[arg(name = "SOCKET")]
    sockets: Vec<String>,
    #[arg(long)]
    directory: String,
    #[clap(long = "allowed-uids", value_delimiter = ',')]
    allowed_uids: Option<Vec<uid_t>>,
}

enum SocketEndpoint {
    UnixSocketEndpoint(String),
    TcpSocketEndpoint(SocketAddr),
}

struct ProxyService {
    socket_endpoints: Vec<SocketEndpoint>,
    directory: String,
    /// Optional allow-list for user IDs.
    allowed_uids: Option<HashSet<uid_t>>,
    cancellation_token: CancellationToken,
    tracker: TaskTracker,
}

impl ProxyService {
    /// Check the socket is allowed.
    ///
    /// This currently checks that the peer user ID is allow-listed.
    fn check_allowed_socket<T: GenericStream>(&self, socket: &T) -> bool {
        match self.allowed_uids {
            None => true,
            Some(ref allowed_uids) => match socket.get_uid() {
                Err(_) => false,
                Ok(ref uid) => allowed_uids.contains(uid),
            },
        }
    }
}

async fn send_reply<T: AsyncWrite + Unpin>(
    socket: &mut T,
    reply: u8,
) -> Result<(), std::io::Error> {
    let reply = [
        SOCKS_VERSION5,
        reply,
        0,
        AddressType::V4 as u8,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    socket.write_all(&reply).await
}

fn is_acceptable_hostname(address: &str) -> bool {
    return !(address.contains('/')
        || address.contains('\\')
        || address.contains(':')
        || address.contains('\0'));
}

async fn serve_socks<T: AsyncRead + AsyncWrite + Unpin>(
    proxy_service: Arc<ProxyService>,
    mut socket: T,
) -> Result<(), Error> {
    let methods = match read_client_hello(&mut socket).await {
        Ok(res) => res,
        Err(err) => {
            debug!("Could not read SOCKS hello");
            return Err(err);
        }
    };
    if !methods.contains(&NO_AUTHENTICATION) {
        info!("SOCKS reply no acceptable authentication");
        let response: [u8; 2] = [SOCKS_VERSION5, NO_ACCEPTABLE_AUTHENTICATION.to_u8()];
        socket.write_all(&response).await?;
        return Ok(());
    }

    let response: [u8; 2] = [SOCKS_VERSION5, NO_AUTHENTICATION.to_u8()];
    socket.write_all(&response).await?;
    let request = match read_socks_request(&mut socket).await {
        Err(err) => {
            debug!("Could not read SOCKS request");
            return Err(err);
        }
        Ok(request) => request,
    };

    info!("{}", request);

    if request.command != COMMAND_CONNECT {
        info!("SOCKS reply, command not supported");
        send_reply(&mut socket, REP_COMMAND_NOT_SUPPORTED).await?;
        return Ok(());
    }

    let requested_domain = match request.address {
        SocksRequestAddress::DomainName(r) => r,
        _ => {
            info!("SOCKS reply, address type not supported)");
            send_reply(&mut socket, REP_ADDRESS_TYPE_NOT_SUPPORTED).await?;
            return Ok(());
        }
    };

    if !is_acceptable_hostname(&requested_domain) {
        info!("SOCKS reply, connection not allowed (invalid domain name)");
        send_reply(&mut socket, REP_CONNECTION_NOT_ALLOWED).await?;
        return Ok(());
    }

    let socket_filename = format!("{}_{}", requested_domain, request.port);
    let socket_path = Path::new(&(*proxy_service).directory.as_str()).join(socket_filename);
    let mut remote_socket = match UnixStream::connect(socket_path).await {
        Ok(res) => res,
        Err(_) => {
            info!("SOCKS reply, not reachable");
            send_reply(&mut socket, REP_HOST_NOT_REACHABLE).await?;
            return Ok(());
        }
    };

    info!("SOCKS reply, succeeded");
    if let Err(err) = send_reply(&mut socket, REP_SUCCEEDED).await {
        return Err(err);
    }

    let _ = copy_bidirectional(&mut socket, &mut remote_socket).await;
    Ok(())
}

#[instrument(skip(proxy_service, socket))]
async fn handle_socks_connection<T: AsyncRead + AsyncWrite + Unpin>(
    proxy_service: Arc<ProxyService>,
    socket: T,
) {
    debug!("New connection");
    if let Err(err) = serve_socks(proxy_service, socket).await {
        debug!(error = display(err));
    }
}

fn make_service() -> ProxyService {
    let args = CliArguments::parse();
    return ProxyService {
        socket_endpoints: args
            .sockets
            .into_iter()
            .map(|endpoint| {
                let parsed: Result<std::net::SocketAddr, AddrParseError> = endpoint.parse();
                match parsed {
                    Err(_) => SocketEndpoint::UnixSocketEndpoint(endpoint),
                    Ok(a) => SocketEndpoint::TcpSocketEndpoint(a),
                }
            })
            .collect(),
        directory: args.directory,
        allowed_uids: args
            .allowed_uids
            .map(|allowed_uids| HashSet::from_iter(allowed_uids.into_iter())),
        cancellation_token: CancellationToken::new(),
        tracker: TaskTracker::new(),
    };
}

async fn accept_unix_socks_connections(proxy_service: Arc<ProxyService>, listener: UnixListener) {
    loop {
        tokio::select! {
            _ = proxy_service.cancellation_token.cancelled() => {
                break;
            },
            listened = listener.accept() => {
                let (socket, _) = match listened {
                    Err(_) => break,
                    Ok(res) => res
                };

                if !proxy_service.check_allowed_socket(&socket) {
                    debug!("Connection rejected");
                    continue;
                }

                let proxy_service3 = proxy_service.clone();
                proxy_service.tracker.spawn(async move {
                    handle_socks_connection(proxy_service3, socket).await;
                });
            }
        }
    }
    proxy_service.cancellation_token.cancel();
    proxy_service.tracker.close();
}

async fn accept_tcp_socks_connections(proxy_service: Arc<ProxyService>, listener: TcpListener) {
    loop {
        tokio::select! {
            _ = proxy_service.cancellation_token.cancelled() => {
                break;
            },
            listened = listener.accept() => {
                let (socket, _) = match listened {
                    Err(_) => break,
                    Ok(res) => res
                };

                if !proxy_service.check_allowed_socket(&socket) {
                    debug!("Connection rejected");
                    continue;
                }

                let proxy_service3 = proxy_service.clone();
                proxy_service.tracker.spawn(async move {
                    handle_socks_connection(proxy_service3, socket).await;
                });
            }
        }
    }
    proxy_service.cancellation_token.cancel();
    proxy_service.tracker.close();
}

async fn start_unix_socket(proxy_service: &Arc<ProxyService>, path: &str) -> Result<(), Error> {
    let listener = UnixListener::bind(path)?;
    let proxy_service2 = proxy_service.clone();
    let path = String::from(path);
    proxy_service.tracker.spawn(async move {
        let _ = accept_unix_socks_connections(proxy_service2, listener).await;
        let _ = remove_file(path).await;
    });
    Ok(())
}

async fn start_tcp_socket(
    proxy_service: &Arc<ProxyService>,
    address: SocketAddr,
) -> Result<(), Error> {
    let listener = TcpListener::bind(address).await?;
    let proxy_service2 = proxy_service.clone();
    proxy_service.tracker.spawn(async move {
        let _ = accept_tcp_socks_connections(proxy_service2, listener).await;
    });
    Ok(())
}

enum AnyListener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

unsafe fn from_raw_fd(fd: RawFd) -> Result<AnyListener, Error> {
    let mut socket_family: c_int = 0;
    let mut socklen: socklen_t = size_of_val(&socket_family) as socklen_t;
    getsockopt(
        fd,
        SOL_SOCKET,
        SO_DOMAIN,
        &mut socket_family as *mut c_int as *mut c_void,
        &mut socklen as *mut socklen_t,
    );

    if socket_family == AF_INET || socket_family == AF_INET6 {
        let raw_listener = std::net::TcpListener::from_raw_fd(fd);
        raw_listener.set_nonblocking(true)?;
        return Ok(AnyListener::Tcp(TcpListener::from_std(raw_listener)?));
    } else {
        let raw_listener = std::os::unix::net::UnixListener::from_raw_fd(fd);
        raw_listener.set_nonblocking(true)?;
        return Ok(AnyListener::Unix(UnixListener::from_std(raw_listener)?));
    }
}

fn handle_socket_activation(proxy_service: &Arc<ProxyService>) -> Result<(), Error> {
    if env::var("LISTEN_PID")
        .ok()
        .and_then(|var| var.parse::<u32>().ok())
        .map_or(true, |listen_pid| listen_pid != std::process::id())
    {
        return Ok(());
    }

    let fd_count = env::var("LISTEN_FDS")
        .ok()
        .and_then(|var| var.parse::<u32>().ok())
        .unwrap_or(0);

    // TODO, handle LISTEN_FDNAMES

    for fd in 3..(3 + fd_count) {
        let listeners;
        unsafe {
            listeners = from_raw_fd(fd as RawFd);
        }

        let proxy_service2 = proxy_service.clone();
        match listeners? {
            AnyListener::Tcp(listener) => {
                info!("Listening to TCP socket #{}", fd);
                proxy_service.tracker.spawn(async move {
                    let _ = accept_tcp_socks_connections(proxy_service2, listener).await;
                })
            }
            AnyListener::Unix(listener) => {
                info!("Listening to Unix domain socket #{}", fd);
                proxy_service.tracker.spawn(async move {
                    let _ = accept_unix_socks_connections(proxy_service2, listener).await;
                })
            }
        };
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let proxy_service = make_service();
    let proxy_service = Arc::new(proxy_service);

    tracing_subscriber::fmt::init();

    handle_socket_activation(&proxy_service)?;

    for socket_enpoint in &(*proxy_service).socket_endpoints {
        match socket_enpoint {
            SocketEndpoint::UnixSocketEndpoint(path) => {
                info!("Listening to Unix domain socket {}", path);
                start_unix_socket(&proxy_service, path.as_str()).await?
            }
            SocketEndpoint::TcpSocketEndpoint(address) => {
                info!("Listening to TCP domain socket {}", *address);
                start_tcp_socket(&proxy_service, *address).await?
            }
        }
    }

    if proxy_service.tracker.is_empty() {
        return Err(Error::from(ErrorKind::Other));
    }

    tokio::select! {
        _ = signal::ctrl_c() => {},
        _ = proxy_service.cancellation_token.cancelled() => {},
    }
    info!("Shutting down");
    proxy_service.cancellation_token.cancel();
    proxy_service.tracker.wait().await;

    Ok(())
}
