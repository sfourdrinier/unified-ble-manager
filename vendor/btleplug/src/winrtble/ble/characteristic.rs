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

use super::{
    super::utils::to_descriptor_value,
    descriptor::{AttributeKey, BLEDescriptor},
};
use crate::{
    api::{Characteristic, WriteType},
    winrtble::utils,
    Error, Result,
};

use log::{trace, warn};
use std::{collections::HashMap, sync::Arc};
use uuid::Uuid;
use windows::core::Ref;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode,
        GenericAttributeProfile::{
            GattCharacteristic, GattCharacteristicProperties,
            GattClientCharacteristicConfigurationDescriptorValue, GattCommunicationStatus,
            GattValueChangedEventArgs, GattWriteOption,
        },
    },
    Foundation::TypedEventHandler,
    Storage::Streams::{DataReader, DataWriter},
};

/// WinRT characteristic value callback.
pub type NotifyEventHandler =
    Arc<dyn Fn(std::result::Result<Vec<u8>, crate::PlatformError>) + Send + Sync>;

impl From<WriteType> for GattWriteOption {
    fn from(val: WriteType) -> Self {
        match val {
            WriteType::WithoutResponse => GattWriteOption::WriteWithoutResponse,
            WriteType::WithResponse => GattWriteOption::WriteWithResponse,
        }
    }
}

/// One `ValueChanged` registration and the handler it forwards to.
struct Subscription {
    token: i64,
    gate: Arc<super::super::callback_gate::CallbackGate>,
}

fn gated_handler(handler: NotifyEventHandler, gate: Arc<super::super::callback_gate::CallbackGate>, faults: Arc<crate::common::util::NotificationFaults>) -> NotifyEventHandler {
    Arc::new(move |value| gate.admit_with_delivery(|| faults.hold_notification_delivery(), || handler(value)))
}

impl std::fmt::Debug for Subscription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Subscription")
            .field("token", &self.token)
            .finish()
    }
}

#[derive(Debug)]
pub struct BLECharacteristic {
    characteristic: GattCharacteristic,
    uuid: Uuid,
    handle: u16,
    properties: GattCharacteristicProperties,
    pub descriptors: HashMap<AttributeKey, BLEDescriptor>,
    subscription: Option<Subscription>,
}

pub(crate) fn value_changed(
    handler: NotifyEventHandler,
) -> TypedEventHandler<GattCharacteristic, GattValueChangedEventArgs> {
    TypedEventHandler::new(
        move |_: Ref<GattCharacteristic>, args: Ref<GattValueChangedEventArgs>| {
            let result = (|| -> windows::core::Result<Vec<u8>> {
                let args = args.ok()?;
                let value = args.CharacteristicValue()?;
                let reader = DataReader::FromBuffer(&value)?;
                let len = reader.UnconsumedBufferLength()? as usize;
                let mut input: Vec<u8> = vec![0u8; len];
                reader.ReadBytes(&mut input[0..len])?;
                trace!("changed {:?}", input);
                Ok(input)
            })();
            handler(result.map_err(|error| {
                crate::PlatformError::new("winrt", "hresult", error.message().to_string()).with(
                    "hresult",
                    crate::winrtble::gatt_model::hresult_code(error.code().0),
                )
            }));
            Ok(())
        },
    )
}

fn cccd_proven_no_effect(status: GattCommunicationStatus, att: Option<u8>) -> bool {
    status == GattCommunicationStatus::ProtocolError && att.is_some()
}

pub(crate) struct CccdWriteFailure {
    pub error: Error,
    pub proven_no_effect: bool,
}

impl BLECharacteristic {
    pub fn new(
        characteristic: GattCharacteristic,
        descriptors: HashMap<AttributeKey, BLEDescriptor>,
    ) -> Result<Self> {
        let uuid = utils::to_uuid(&characteristic.Uuid()?);
        let handle = characteristic.AttributeHandle()?;
        let properties = characteristic.CharacteristicProperties()?;
        Ok(BLECharacteristic {
            characteristic,
            uuid,
            handle,
            properties,
            descriptors,
            subscription: None,
        })
    }

    pub fn key(&self) -> AttributeKey {
        (self.uuid, u64::from(self.handle))
    }

    pub fn gatt(&self) -> &GattCharacteristic {
        &self.characteristic
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscription.is_some()
    }

    pub async fn write_value(
        characteristic: &GattCharacteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<()> {
        Self::start_write_value(characteristic, data, write_type)?.await
    }

    /// Creates the WinRT operation synchronously; the returned future owns
    /// completion and must be retained independently of the requesting task.
    pub(crate) fn start_write_value(
        characteristic: &GattCharacteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>> {
        Self::start_write_value_inner(characteristic, data, write_type)
    }

    pub(crate) fn start_marker_write_value(
        characteristic: &GattCharacteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>> {
        Self::start_write_value_inner(characteristic, data, write_type)
    }

    fn start_write_value_inner(
        characteristic: &GattCharacteristic,
        data: &[u8],
        write_type: WriteType,
    ) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>> {
        let writer = DataWriter::new()?;
        writer.WriteBytes(data)?;
        if write_type == WriteType::WithoutResponse {
            // An ATT Write Command has no response, so no ATT error can
            // come back: only the transport status is reported.
            let operation = {
                // `IBuffer` is not `Send`: the buffer drops before the
                // await, so the future stays `Send`.
                let buffer = writer.DetachBuffer()?;
                characteristic.WriteValueWithOptionAsync(&buffer, write_type.into())?
            };
            return Ok(Box::pin(async move {
                let status = super::terminal_result(operation).await?;
                if status == GattCommunicationStatus::Success {
                    return Ok(());
                }
                Err(utils::gatt_status_error(
                    "Gatt characteristic write",
                    status,
                    None,
                ))
            }));
        }
        // UBM patch (UBM_PATCHES.md #20): a with-response write goes
        // through the result-returning call (the same ATT Write Request on
        // the wire), so a `ProtocolError` status carries the ATT error
        // byte and the host can tell a security refusal apart. The buffer
        // drops before the await, so the future stays `Send`.
        let operation = {
            let buffer = writer.DetachBuffer()?;
            characteristic.WriteValueWithResultAndOptionAsync(&buffer, write_type.into())?
        };
        Ok(Box::pin(async move {
            let result = super::terminal_result(operation).await?;
            let status = result.Status()?;
            if status == GattCommunicationStatus::Success {
                Ok(())
            } else {
                let mut att = None;
                if status == GattCommunicationStatus::ProtocolError {
                    att = utils::protocol_att_error(result.ProtocolError());
                }
                Err(utils::gatt_status_error(
                    "Gatt characteristic write",
                    status,
                    att,
                ))
            }
        }))
    }

    pub(crate) fn start_read_value(
        characteristic: &GattCharacteristic,
    ) -> Result<std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<u8>>> + Send>>> {
        let operation = characteristic.ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)?;
        Ok(Box::pin(async move {
            let result = super::terminal_result(operation).await?;
            let status = result.Status()?;
            if status == GattCommunicationStatus::Success {
                let value = result.Value()?;
                let reader = DataReader::FromBuffer(&value)?;
                let len = reader.UnconsumedBufferLength()? as usize;
                let mut input = vec![0u8; len];
                reader.ReadBytes(&mut input[0..len])?;
                Ok(input)
            } else {
                // UBM patch (UBM_PATCHES.md #20): a `ProtocolError` status
                // carries the ATT error byte, so the host can tell a security
                // refusal from any other protocol error.
                let mut att = None;
                if status == GattCommunicationStatus::ProtocolError {
                    att = utils::protocol_att_error(result.ProtocolError());
                }
                Err(utils::gatt_status_error(
                    "Gatt characteristic read",
                    status,
                    att,
                ))
            }
        }))
    }

    /// Write this characteristic's CCCD. `operation` names the request in
    /// the error when the peer or the OS refuses it.
    pub async fn write_client_configuration(
        characteristic: &GattCharacteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
        operation: &str,
    ) -> Result<()> {
        Self::write_client_configuration_classified(characteristic, value, operation)
            .await
            .map_err(|failure| failure.error)
    }

    pub(crate) async fn write_client_configuration_classified(
        characteristic: &GattCharacteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
        operation: &str,
    ) -> std::result::Result<(), CccdWriteFailure> {
        let operation_handle = Self::start_client_configuration(characteristic, value)?;
        Self::finish_client_configuration(operation_handle, operation).await
    }

    pub(crate) fn start_client_configuration(
        characteristic: &GattCharacteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
    ) -> std::result::Result<
        windows_future::IAsyncOperation<
            windows::Devices::Bluetooth::GenericAttributeProfile::GattWriteResult,
        >,
        CccdWriteFailure,
    > {
        characteristic
            .WriteClientCharacteristicConfigurationDescriptorWithResultAsync(value)
            .map_err(|error| CccdWriteFailure {
                error: error.into(),
                proven_no_effect: false,
            })
    }

    pub(crate) async fn finish_client_configuration(
        operation_handle: windows_future::IAsyncOperation<
            windows::Devices::Bluetooth::GenericAttributeProfile::GattWriteResult,
        >,
        operation: &str,
    ) -> std::result::Result<(), CccdWriteFailure> {
        let unknown = |error: windows::core::Error| CccdWriteFailure {
            error: error.into(),
            proven_no_effect: false,
        };
        let result = super::terminal_result(operation_handle).await.map_err(unknown)?;
        let status = result.Status().map_err(unknown)?;
        trace!("{operation} {:?}", status);
        if status == GattCommunicationStatus::Success {
            Ok(())
        } else {
            let att = utils::protocol_att_error(result.ProtocolError());
            Err(CccdWriteFailure {
                error: utils::gatt_status_error(operation, status, att),
                // A received ATT error response proves the request was refused.
                proven_no_effect: cccd_proven_no_effect(status, att),
            })
        }
    }

    /// Register `handler` for `ValueChanged` and return the CCCD value
    /// `to_descriptor_value` would write (Indicate when that bit is set).
    /// Other callers keep that path. The desktop enable path uses
    /// [`register_with`] so the first write is the selected mode.
    pub fn register(
        &mut self,
        handler: NotifyEventHandler,
        faults: Arc<crate::common::util::NotificationFaults>,
    ) -> Result<(GattClientCharacteristicConfigurationDescriptorValue, i64)> {
        self.register_with(handler, to_descriptor_value(self.properties), faults)
    }

    /// Register `handler` and write `config` once. `None` is refused before
    /// anything is registered. The given value is not replaced with the
    /// other mode.
    pub fn register_with(
        &mut self,
        handler: NotifyEventHandler,
        config: GattClientCharacteristicConfigurationDescriptorValue,
        faults: Arc<crate::common::util::NotificationFaults>,
    ) -> Result<(GattClientCharacteristicConfigurationDescriptorValue, i64)> {
        if config == GattClientCharacteristicConfigurationDescriptorValue::None {
            return Err(Error::NotSupported("Can not subscribe to attribute".into()));
        }
        self.take_registration()?;
        let gate = Arc::new(super::super::callback_gate::CallbackGate::new(true));
        let token = self
            .characteristic
            .ValueChanged(&value_changed(gated_handler(Arc::clone(&handler), gate.clone(), faults.clone())))?;
        self.subscription = Some(Subscription { token, gate });
        Ok((config, token))
    }

    /// Remove the registration `token` if it is still this object's
    /// current one (a failed subscribe rolls back only its own handler).
    pub fn deregister(&mut self, token: i64) -> Result<()> {
        if self
            .subscription
            .as_ref()
            .is_some_and(|subscription| subscription.token == token)
        {
            self.take_registration()?;
        }
        Ok(())
    }

    /// Remove the current `ValueChanged` registration, if any.
    pub fn take_registration(&mut self) -> Result<()> {
        if let Some(subscription) = self.subscription.take() {
            if let Err(error) = self.characteristic.RemoveValueChanged(subscription.token) {
                self.subscription = Some(subscription);
                return Err(error.into());
            }
        }
        Ok(())
    }

    pub(in crate::winrtble) fn callback_gate(&self) -> Option<Arc<super::super::callback_gate::CallbackGate>> {
        self.subscription.as_ref().map(|subscription| subscription.gate.clone())
    }

    pub fn to_characteristic(&self, service_uuid: Uuid, service_instance: u64) -> Characteristic {
        let instance = u64::from(self.handle);
        let descriptors = self
            .descriptors
            .values()
            .map(|descriptor| {
                descriptor.to_descriptor(service_uuid, service_instance, self.uuid, instance)
            })
            .collect();
        Characteristic {
            uuid: self.uuid,
            instance,
            service_uuid,
            service_instance,
            descriptors,
            properties: utils::to_char_props(&self.properties),
        }
    }
}

impl Drop for BLECharacteristic {
    fn drop(&mut self) {
        if let Err(err) = self.take_registration() {
            warn!(
                "Drop: removing the ValueChanged handler of {} (handle {}) failed: {:?}",
                self.uuid, self.handle, err
            );
        }
    }
}

#[cfg(test)]
mod cccd_outcome_tests {
    use super::{cccd_proven_no_effect, GattCommunicationStatus};

    #[test]
    fn only_att_refusals_prove_cccd_write_had_no_effect() {
        assert!(cccd_proven_no_effect(
            GattCommunicationStatus::ProtocolError,
            Some(5)
        ));
        assert!(!cccd_proven_no_effect(
            GattCommunicationStatus::ProtocolError,
            None
        ));
        assert!(!cccd_proven_no_effect(
            GattCommunicationStatus::Unreachable,
            None
        ));
        assert!(!cccd_proven_no_effect(
            GattCommunicationStatus::AccessDenied,
            None
        ));
    }
}
