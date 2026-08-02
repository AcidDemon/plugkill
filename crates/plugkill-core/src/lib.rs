pub mod config;
pub mod display;
pub mod error;
pub mod ipc;
pub mod lid;
pub mod network;
pub mod pci;
pub mod power;
pub mod sdcard;
pub mod state;
pub mod sysfs;
pub mod thunderbolt;
pub mod usb;

#[cfg(target_os = "freebsd")]
pub mod platform_freebsd;
