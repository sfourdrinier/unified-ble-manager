//! The vendored WinRT GATT rules (`vendor/btleplug/src/winrtble/gatt_model.rs`,
//! UBM patch `winrt-attribute-instances`) are pure std code. The WinRT
//! backend compiles only for Windows, so this file compiles the same source
//! on every host and runs its own unit tests here.

#[path = "../../../vendor/btleplug/src/winrtble/gatt_model.rs"]
mod gatt_model;
