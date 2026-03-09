use std::fmt;
use std::io;

/// Central error type for usbkill operations.
#[derive(Debug)]
pub enum Error {
    /// Configuration file could not be read or parsed.
    Config(String),
    /// USB device enumeration failed.
    Usb(String),
    /// Kill sequence encountered an error.
    Kill(String),
    /// An I/O operation failed.
    Io(io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(msg) => write!(f, "configuration error: {msg}"),
            Error::Usb(msg) => write!(f, "USB detection error: {msg}"),
            Error::Kill(msg) => write!(f, "kill sequence error: {msg}"),
            Error::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}
