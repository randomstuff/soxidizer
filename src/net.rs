use libc::uid_t;
use std::{
    io::{Error, ErrorKind},
    net::SocketAddr as InetAddr,
};
use tokio::net::{unix::SocketAddr as UnixAddr, TcpListener, TcpStream, UnixListener, UnixStream};

pub enum Stream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

pub enum Addr {
    Inet(InetAddr),
    Unix(UnixAddr),
}

pub enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

impl Listener {
    pub async fn accept(&self) -> Result<(Stream, Addr), Error> {
        match self {
            Listener::Tcp(listener) => {
                let (s, a) = listener.accept().await?;
                Ok((Stream::Tcp(s), Addr::Inet(a)))
            }
            Listener::Unix(listener) => {
                let (s, a) = listener.accept().await?;
                Ok((Stream::Unix(s), Addr::Unix(a)))
            }
        }
    }
}

pub trait GetUid {
    fn get_uid(&self) -> Result<uid_t, std::io::Error>;
}

impl GetUid for UnixStream {
    fn get_uid(&self) -> Result<uid_t, std::io::Error> {
        self.peer_cred().map(|cred| cred.uid())
    }
}

impl GetUid for TcpStream {
    fn get_uid(&self) -> Result<uid_t, std::io::Error> {
        // Not implemented yet:
        return Err(Error::from(ErrorKind::Other));
    }
}

impl GetUid for Stream {
    fn get_uid(&self) -> Result<uid_t, std::io::Error> {
        match self {
            Stream::Tcp(s) => s.get_uid(),
            Stream::Unix(s) => s.get_uid(),
        }
    }
}
