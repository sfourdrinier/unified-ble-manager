pub mod error;
pub mod gatt;
pub mod uuid;
pub mod notification;

mod peripheral;
pub use self::peripheral::{Peripheral, PeripheralImpl};
