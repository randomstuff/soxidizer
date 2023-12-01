mod socks;

use std::io::Error;
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
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
use tokio::io::copy_bidirectional;
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
};
use tracing::instrument;
use tracing::{debug, info};

use crate::socks::SocksRequestAddress;
use crate::socks::REP_ADDRESS_TYPE_NOT_SUPPORTED;
use crate::socks::{REP_COMMAND_NOT_SUPPORTED, REP_CONNECTION_NOT_ALLOWED, REP_HOST_NOT_REACHABLE};

#[derive(Parser, Debug)]
struct CliArguments {
    #[arg(long)]
    socket: String,
    #[arg(long)]
    directory: String,
}

struct ProxyService {
    socket_path: String,
    directory: String,
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
    return ProxyService {
        socket_path: args.socket,
        directory: args.directory,
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

    loop {
        let proxy_service = proxy_service.clone();
        let (socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            handle_socks_connection(proxy_service, socket).await;
        });
    }
}
