use std::fmt;
use std::fmt::Display;
use std::io::{Error, ErrorKind, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str;

use tokio::io::AsyncReadExt;
use tokio::io::{AsyncRead, AsyncWrite};

pub static SOCKS_VERSION5: u8 = 5;

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct AuthenticationMethod(u8);

impl AuthenticationMethod {
    pub fn to_u8(&self) -> u8 {
        let AuthenticationMethod(raw) = self;
        *raw
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct SocksCommand(u8);

pub static COMMAND_CONNECT: SocksCommand = SocksCommand(1);
pub static COMMAND_BIND: SocksCommand = SocksCommand(2);
pub static COMMAND_UDP_ASSOCIATE: SocksCommand = SocksCommand(3);

pub static REP_SUCCEEDED: u8 = 0;
pub static REP_CONNECTION_NOT_ALLOWED: u8 = 2;
pub static REP_HOST_NOT_REACHABLE: u8 = 4;
pub static REP_COMMAND_NOT_SUPPORTED: u8 = 7;
pub static REP_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 8;

impl SocksCommand {
    #[allow(dead_code)]
    pub fn to_u8(&self) -> u8 {
        let SocksCommand(raw) = self;
        *raw
    }
}

pub static NO_AUTHENTICATION: AuthenticationMethod = AuthenticationMethod(0);
pub static NO_ACCEPTABLE_AUTHENTICATION: AuthenticationMethod = AuthenticationMethod(255);

pub async fn read_client_hello<T: AsyncRead + AsyncWrite + Unpin>(
    read: &mut T,
) -> Result<Vec<AuthenticationMethod>> {
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

#[repr(u8)]
pub enum AddressType {
    V4 = 1,
    DOMAINNAME = 3,
    V6 = 4,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum SocksRequestAddress {
    IpAddress(IpAddr),
    DomainName(String),
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct SocksRequest {
    pub command: SocksCommand,
    pub address: SocksRequestAddress,
    pub port: u16,
}

fn get_command_name(command: SocksCommand) -> &'static str {
    if command == COMMAND_CONNECT {
        return "CONNECT";
    } else if command == COMMAND_BIND {
        return "BIND";
    } else if command == COMMAND_UDP_ASSOCIATE {
        return "UDP_ASSOCIATE";
    } else {
        return "?";
    }
}

impl Display for SocksCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", get_command_name(*self))
    }
}

impl Display for SocksRequestAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SocksRequestAddress::IpAddress(a) => write!(f, "{}", a),
            SocksRequestAddress::DomainName(d) => write!(f, "{}", d),
        }
    }
}

impl Display for SocksRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SOCKS {} {} {}", self.command, self.address, self.port)
    }
}

pub async fn read_socks_request<T: AsyncRead + AsyncWrite + Unpin>(
    read: &mut T,
) -> Result<SocksRequest> {
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

        if version != SOCKS_VERSION5 {
            return Err(Error::from(ErrorKind::Other));
        }

        let request_address: SocksRequestAddress;
        let port_offset: usize;
        if atype == AddressType::DOMAINNAME as u8 {
            let domain_length = usize::from(buffer[4]);
            if total_read < domain_length + 7 {
                continue;
            }

            let raw_address = str::from_utf8(&buffer[5..(5 + domain_length)]);
            request_address = match raw_address {
                Err(_) => return Err(Error::from(ErrorKind::Other)),
                Ok(address) => SocksRequestAddress::DomainName(String::from(address)),
            };

            port_offset = 5 + domain_length;
        } else if atype == AddressType::V4 as u8 {
            if total_read < 10 {
                continue;
            }
            let mut raw_address: [u8; 4] = [0; 4];
            raw_address.copy_from_slice(&buffer[4..8]);
            request_address =
                SocksRequestAddress::IpAddress(IpAddr::V4(Ipv4Addr::from(raw_address)));
            port_offset = 8;
        } else if atype == AddressType::V6 as u8 {
            if total_read < 22 {
                continue;
            }
            let mut raw_address: [u8; 16] = [0; 16];
            raw_address.copy_from_slice(&buffer[4..20]);
            request_address =
                SocksRequestAddress::IpAddress(IpAddr::V6(Ipv6Addr::from(raw_address)));
            port_offset = 20;
        } else {
            return Err(Error::from(ErrorKind::Other));
        }

        let port = u16::from_be_bytes([buffer[port_offset], buffer[port_offset + 1]]);

        return Ok(SocksRequest {
            command: command,
            address: request_address,
            port: port,
        });
    }
    return Err(Error::from(ErrorKind::Other));
}
