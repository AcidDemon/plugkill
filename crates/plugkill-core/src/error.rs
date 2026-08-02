use std::fmt;
use std::io;

/// Central error type for plugkill operations.
#[derive(Debug)]
pub enum Error {
    /// Configuration file could not be read or parsed.
    Config(String),
    /// USB device enumeration failed.
    Usb(String),
    /// Thunderbolt device enumeration failed.
    Thunderbolt(String),
    /// SD card enumeration failed.
    SdCard(String),
    /// Power supply monitoring failed.
    #[allow(dead_code)]
    Power(String),
    /// Network interface monitoring failed.
    #[allow(dead_code)]
    Network(String),
    /// Lid state monitoring failed.
    #[allow(dead_code)]
    Lid(String),
    /// Kill sequence encountered an error.
    Kill(String),
    /// Socket-related error.
    #[allow(dead_code)]
    Socket(String),
    /// An I/O operation failed.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "configuration error: {msg}"),
            Error::Usb(msg) => write!(f, "USB detection error: {msg}"),
            Error::Thunderbolt(msg) => write!(f, "Thunderbolt detection error: {msg}"),
            Error::SdCard(msg) => write!(f, "SD card detection error: {msg}"),
            Error::Power(msg) => write!(f, "power monitoring error: {msg}"),
            Error::Network(msg) => write!(f, "network monitoring error: {msg}"),
            Error::Lid(msg) => write!(f, "lid monitoring error: {msg}"),
            Error::Kill(msg) => write!(f, "kill sequence error: {msg}"),
            Error::Socket(msg) => write!(f, "socket error: {msg}"),
            Error::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            Error::Config(_)
            | Error::Usb(_)
            | Error::Thunderbolt(_)
            | Error::SdCard(_)
            | Error::Power(_)
            | Error::Network(_)
            | Error::Lid(_)
            | Error::Kill(_)
            | Error::Socket(_) => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}
