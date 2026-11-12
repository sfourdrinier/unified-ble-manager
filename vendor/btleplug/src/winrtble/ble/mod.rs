// btleplug Source Code File
//
// Copyright 2020 Nonpolynomial Labs LLC. All rights reserved.
//
// Licensed under the BSD 3-Clause license. See LICENSE file in the project root
// for full license information.
//
// Some portions of this file are taken and/or modified from Rumble
// (https://github.com/mwylde/rumble), using a dual MIT/Apache License under the
// following copyright:
//
// Copyright (c) 2014 The Rust Project Developers

pub mod characteristic;
pub mod descriptor;
pub mod device;
pub mod service;
pub mod watcher;

// A failed completion registration is not a native terminal result. Retain the
// exact operation until Windows exposes completion; do not cancel it. Canceled
// is also not a quiescence proof for GATT initialization.
pub(super) async fn terminal_result<T: windows::core::RuntimeType + 'static>(
    operation: windows_future::IAsyncOperation<T>,
) -> windows::core::Result<T> {
    use std::future::IntoFuture;
    match operation.clone().into_future().await {
        Ok(result) => Ok(result),
        Err(error) => {
            log::warn!("WinRT completion needs a native status fence: {error}");
            super::marker_executor::wait_for_terminal(|| {
                use windows_future::AsyncStatus;
                match operation.Status() {
                    Ok(AsyncStatus::Completed | AsyncStatus::Error) => Some(operation.GetResults()),
                    _ => None,
                }
            }).await
        }
    }
}
