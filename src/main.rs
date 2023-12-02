mod socks;

use std::collections::HashSet;
use std::io::Error;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use libc::getuid;
use libc::S_IRGRP;
use libc::S_IROTH;
use libc::S_IWGRP;
use libc::S_IWOTH;
use libc::S_IXGRP;
use libc::S_IXOTH;
use socks::AddressType;
use socks::REP_SUCCEEDED;
use socks::{
    read_client_hello, read_socks_request, COMMAND_CONNECT, NO_ACCEPTABLE_AUTHENTICATION,
    NO_AUTHENTICATION, SOCKS_VERSION5,
};
use tokio::fs::remove_file;
use tokio::io::copy_bidirectional;
use tokio::net::unix::uid_t;
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

#[derive(Parser, Debug)]
struct CliArguments {
    #[arg(name = "SOCKET")]
    socket: String,
    #[arg(long)]
    directory: String,
    #[clap(long = "allowed-uids", value_delimiter = ',')]
    allowed_uids: Option<Vec<uid_t>>,
    #[clap(long = "unfiltered", num_args = 0)]
    unfiltered: bool,
}

struct ProxyService {
    socket_path: String,
    directory: String,
    /// Optional allow-list for user IDs.
    allowed_uids: HashSet<uid_t>,
    unfiltered: bool,
}

impl ProxyService {
    /// Check the socket is allowed.
    ///
    /// This currently checks that the peer user ID is allow-listed.
    fn check_allowed_socket(&self, socket: &UnixStream) -> bool {
        if self.unfiltered {
            return true;
        }
        match socket.peer_cred() {
            Err(_) => false,
            Ok(cred) => self.allowed_uids.contains(&cred.uid()),
        }
    }
}

async fn send_reply(socket: &mut UnixStream, reply: u8) -> Result<(), std::io::Error> {
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

async fn serve_socks(
    proxy_service: Arc<ProxyService>,
    mut socket: UnixStream,
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
async fn handle_socks_connection(proxy_service: Arc<ProxyService>, socket: UnixStream) {
    debug!("New connection");
    if let Err(err) = serve_socks(proxy_service, socket).await {
        debug!(error = display(err));
    }
}

fn make_service() -> ProxyService {
    let args = CliArguments::parse();

    let uid: uid_t;
    unsafe {
        uid = getuid();
    }

    return ProxyService {
        socket_path: args.socket,
        directory: args.directory,
        allowed_uids: HashSet::from([uid]),
        unfiltered: args.unfiltered,
    };
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let proxy_service = make_service();

    tracing_subscriber::fmt::init();

    // HACK: On Linux, this makes sure the socket is not accessible by other users.
    // - We could stduse ::fs::set_permissions after creating the socket
    //   but the socket would be connectable for a short period of time.
    // - We could use a chmod() after bind() and before listen()
    //   but the Rust API does not allow us to do that.
    let original_mode;
    unsafe {
        original_mode = libc::umask(S_IRGRP | S_IWGRP | S_IXGRP | S_IROTH | S_IWOTH | S_IXOTH);
    };
    let listener = UnixListener::bind(proxy_service.socket_path.as_str())?;
    unsafe {
        libc::umask(original_mode);
    }

    let proxy_service = Arc::new(proxy_service);
    let token = CancellationToken::new();
    let tracker = TaskTracker::new();

    let proxy_service2 = proxy_service.clone();
    let token2 = token.clone();
    let tracker2 = tracker.clone();

    tracker.spawn(async move {
        loop {
            tokio::select! {
                _ = token2.cancelled() => {
                    break;
                },
                listened = listener.accept() => {
                    let (socket, _) = match listened {
                        Err(_) => break,
                        Ok(res) => res
                    };

                    if !(proxy_service2).check_allowed_socket(&socket) {
                        debug!("Connection rejected");
                        continue;
                    }

                    let proxy_service = proxy_service2.clone();
                    tracker2.spawn(async move {
                        handle_socks_connection(proxy_service, socket).await;
                    });
                }
            }
        }
        token2.cancel();
        tracker2.close();
    });

    tokio::select! {
        _ = signal::ctrl_c() => {},
        _ = token.cancelled() => {},
    }
    info!("Shutting down");
    let _ = remove_file(proxy_service.socket_path.as_str()).await;
    token.cancel();
    tracker.wait().await;

    Ok(())
}
