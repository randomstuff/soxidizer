mod socks;

use std::io::{Error, ErrorKind};
use std::path::Path;
use std::sync::Arc;

use clap::Parser;
use libc::S_IRGRP;
use libc::S_IROTH;
use libc::S_IWGRP;
use libc::S_IWOTH;
use libc::S_IXGRP;
use libc::S_IXOTH;
use socks::ATYPE_IPV4;
use socks::REP_SUCCEEDED;
use socks::{
    read_client_hello, read_socks_request, CONNECT, NO_ACCEPTABLE_AUTHENTICATION,
    NO_AUTHENTICATION, SOCKS_VERSION5,
};
use tokio::io::AsyncReadExt;
use tokio::net::unix::OwnedReadHalf;
use tokio::net::unix::OwnedWriteHalf;
use tokio::{
    io::AsyncWriteExt,
    net::{UnixListener, UnixStream},
};
use tracing::instrument;

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

async fn communicate_oneway<'a>(mut read: OwnedReadHalf, mut write: OwnedWriteHalf) {
    let mut buffer: Vec<u8> = vec![0; 4096];
    loop {
        let read_result = read.read(buffer.as_mut_slice()).await;
        let read_count = match read_result {
            Err(_) => 0,
            Ok(read_count) => read_count,
        };
        if read_count == 0 {
            let _ = write.shutdown().await;
            return;
        }
        let write_res = write.write_all(&buffer[0..read_count]).await;
        if write_res.is_err() {
            let _ = write.shutdown().await;
            return;
        }
    }
}

async fn communicate(socket1: UnixStream, socket2: UnixStream) {
    let (read1, write1) = socket1.into_split();
    let (read2, write2) = socket2.into_split();

    tokio::spawn(async move {
        communicate_oneway(read1, write2).await;
    });
    tokio::spawn(async move {
        communicate_oneway(read2, write1).await;
    });
}

#[instrument(skip(proxy_service, socket))]
async fn serve_socks(
    proxy_service: Arc<ProxyService>,
    mut socket: UnixStream,
) -> Result<(), Error> {
    let methods = read_client_hello(&mut socket).await?;
    if !methods.contains(&NO_AUTHENTICATION) {
        let response: [u8; 2] = [SOCKS_VERSION5, NO_ACCEPTABLE_AUTHENTICATION.to_u8()];
        socket.write_all(&response).await?;
        return Ok(());
    }
    let response: [u8; 2] = [SOCKS_VERSION5, NO_AUTHENTICATION.to_u8()];
    socket.write_all(&response).await?;
    let request = read_socks_request(&mut socket).await;
    let request = match request {
        // TODO, return error to client
        Err(_) => return Err(Error::from(ErrorKind::Other)),
        Ok(request) => request,
    };

    if request.command != CONNECT {
        // TODO, return error to client
        return Err(Error::from(ErrorKind::Other));
    }

    if request.address.contains('/') || request.address.contains('\\') {
        return Err(Error::from(ErrorKind::Other));
    }

    let socket_filename = format!("{}_{}", request.address, request.port);
    let socket_path = Path::new(&(*proxy_service).directory.as_str()).join(socket_filename);
    let remote_socket = UnixStream::connect(socket_path).await?;

    let response: [u8; 10] = [
        SOCKS_VERSION5,
        REP_SUCCEEDED,
        0,
        ATYPE_IPV4,
        127,
        0,
        0,
        1,
        0,
        0,
    ];
    socket.write_all(&response).await?;

    communicate(socket, remote_socket).await;
    Ok(())
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
    // HACK: On Linux, this makes sure the socket is not accessible by other users.
    // In could use a chmod() after bind() but before listen()
    // but the Rust API does not allow us to do that.
    unsafe {
        libc::umask(S_IRGRP | S_IWGRP | S_IXGRP | S_IROTH | S_IWOTH | S_IXOTH);
    }

    let proxy_service = make_service();

    tracing_subscriber::fmt::init();

    let listener = UnixListener::bind(proxy_service.socket_path.as_str())?;

    let proxy_service = Arc::new(proxy_service);

    loop {
        let proxy_service = proxy_service.clone();
        let (socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            if let Err(_err) = serve_socks(proxy_service, socket).await {
                println!("ERR");
                // TODO,  log error
            }
        });
    }
}
