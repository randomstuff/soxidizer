use std::io::{Error, ErrorKind, Result};
use std::str;

use tokio::{io::AsyncReadExt, net::UnixStream};

pub static SOCKS_VERSION5: u8 = 5;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct AuthenticationMethod(u8);

impl AuthenticationMethod {
    pub fn to_u8(&self) -> u8 {
        let AuthenticationMethod(raw) = self;
        *raw
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SocksCommand(u8);

pub static CONNECT: SocksCommand = SocksCommand(1);

pub static REP_SUCCEEDED: u8 = 0;

impl SocksCommand {
    #[allow(dead_code)]
    pub fn to_u8(&self) -> u8 {
        let SocksCommand(raw) = self;
        *raw
    }
}

pub static NO_AUTHENTICATION: AuthenticationMethod = AuthenticationMethod(0);
pub static NO_ACCEPTABLE_AUTHENTICATION: AuthenticationMethod = AuthenticationMethod(255);

pub async fn read_client_hello(read: &mut UnixStream) -> Result<Vec<AuthenticationMethod>> {
    let mut buffer: [u8; 256] = [0; 256];
    let mut total_read: usize = 0;
    while total_read < buffer.len() {
        let read_count = read.read(&mut buffer[total_read..]).await?;
        if read_count == 0 {
            return Err(Error::from(ErrorKind::Other));
        }
        total_read += read_count;
        if total_read < 2 {
            continue;
        }
        let version = buffer[0];
        if version != SOCKS_VERSION5 {
            return Err(Error::from(ErrorKind::Other));
        }
        let method_count = usize::from(buffer[1]);
        if method_count + 2 < total_read {
            continue;
        }
        let slice: &[u8] = &buffer[2..(method_count + 2)];
        let res = Vec::from_iter(slice.iter().map(|x| AuthenticationMethod(*x)));
        return Ok(res);
    }
    return Err(Error::from(ErrorKind::Other));
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SocksRequest {
    pub command: SocksCommand,
    pub address: String,
    pub port: u16,
}

#[allow(dead_code)]
pub const ATYPE_IPV4: u8 = 1;
pub const ATYPE_DOMAINNAME: u8 = 3;
#[allow(dead_code)]
pub const ATYPE_IPV6: u8 = 4;

pub async fn read_socks_request(read: &mut UnixStream) -> Result<SocksRequest> {
    let mut buffer: [u8; 512] = [0; 512];
    let mut total_read: usize = 0;
    while total_read < buffer.len() {
        let read_count = read.read(&mut buffer[total_read..]).await?;
        if read_count == 0 {
            return Err(Error::from(ErrorKind::Other));
        }
        total_read = total_read + read_count;
        if total_read < 7 {
            continue;
        }

        let version = buffer[0];
        let command = SocksCommand(buffer[1]);
        let _reserved = buffer[2];
        let atype = buffer[3];

        if version != SOCKS_VERSION5 || atype != ATYPE_DOMAINNAME {
            return Err(Error::from(ErrorKind::Other));
        }

        let domain_length = usize::from(buffer[4]);
        if total_read < domain_length + 7 {
            continue;
        }

        let address = str::from_utf8(&buffer[5..(5 + domain_length)]);
        let address = match address {
            Err(_) => return Err(Error::from(ErrorKind::Other)),
            Ok(address) => address,
        };
        let port = [buffer[5 + domain_length], buffer[5 + domain_length + 1]];
        let port = u16::from_be_bytes(port);

        return Ok(SocksRequest {
            command: command,
            address: address.to_string(),
            port: port,
        });
    }
    return Err(Error::from(ErrorKind::Other));
}
