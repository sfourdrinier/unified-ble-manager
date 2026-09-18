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
    Error, Result,
    api::{Characteristic, WriteType},
    winrtble::utils,
};

use log::{trace, warn};
use std::{collections::HashMap, future::IntoFuture, sync::Arc};
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

/// UBM patch (`winrt-attribute-instances`): shared so a live subscription
/// can move to the characteristic object of a later discovery.
pub type NotifyEventHandler = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

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
    handler: NotifyEventHandler,
    token: i64,
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

fn value_changed(
    handler: NotifyEventHandler,
) -> TypedEventHandler<GattCharacteristic, GattValueChangedEventArgs> {
    TypedEventHandler::new(
        move |_: Ref<GattCharacteristic>, args: Ref<GattValueChangedEventArgs>| {
            if let Ok(args) = args.ok() {
                let value = args.CharacteristicValue()?;
                let reader = DataReader::FromBuffer(&value)?;
                let len = reader.UnconsumedBufferLength()? as usize;
                let mut input: Vec<u8> = vec![0u8; len];
                reader.ReadBytes(&mut input[0..len])?;
                trace!("changed {:?}", input);
                handler(input);
            }
            Ok(())
        },
    )
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
        let writer = DataWriter::new()?;
        writer.WriteBytes(data)?;
        let operation =
            characteristic.WriteValueWithOptionAsync(&writer.DetachBuffer()?, write_type.into())?;
        let result = operation.into_future().await?;
        if result == GattCommunicationStatus::Success {
            Ok(())
        } else {
            Err(utils::gatt_status_error(
                "Gatt characteristic write",
                result,
            ))
        }
    }

    pub async fn read_value(characteristic: &GattCharacteristic) -> Result<Vec<u8>> {
        let result = characteristic
            .ReadValueWithCacheModeAsync(BluetoothCacheMode::Uncached)?
            .into_future()
            .await?;
        let status = result.Status()?;
        if status == GattCommunicationStatus::Success {
            let value = result.Value()?;
            let reader = DataReader::FromBuffer(&value)?;
            let len = reader.UnconsumedBufferLength()? as usize;
            let mut input = vec![0u8; len];
            reader.ReadBytes(&mut input[0..len])?;
            Ok(input)
        } else {
            Err(utils::gatt_status_error("Gatt characteristic read", status))
        }
    }

    /// Write this characteristic's CCCD. `operation` names the request in
    /// the error when the peer or the OS refuses it.
    pub async fn write_client_configuration(
        characteristic: &GattCharacteristic,
        value: GattClientCharacteristicConfigurationDescriptorValue,
        operation: &str,
    ) -> Result<()> {
        let status = characteristic
            .WriteClientCharacteristicConfigurationDescriptorAsync(value)?
            .into_future()
            .await?;
        trace!("{operation} {:?}", status);
        if status == GattCommunicationStatus::Success {
            Ok(())
        } else {
            Err(utils::gatt_status_error(operation, status))
        }
    }

    /// Register `handler` for `ValueChanged`, replacing this object's
    /// previous registration so handlers never accumulate, and return the
    /// CCCD value subscribe writes with the new registration's token. A
    /// characteristic that can neither notify nor indicate is refused
    /// before anything is registered.
    pub fn register(
        &mut self,
        handler: NotifyEventHandler,
    ) -> Result<(GattClientCharacteristicConfigurationDescriptorValue, i64)> {
        let config = to_descriptor_value(self.properties);
        if config == GattClientCharacteristicConfigurationDescriptorValue::None {
            return Err(Error::NotSupported("Can not subscribe to attribute".into()));
        }
        self.take_registration()?;
        let token = self
            .characteristic
            .ValueChanged(&value_changed(Arc::clone(&handler)))?;
        self.subscription = Some(Subscription { handler, token });
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

    /// Move `previous`'s live subscription onto this object, the same
    /// attribute (UUID and handle) returned by a later discovery. The new
    /// registration is made before the old one is removed, so no value is
    /// lost in between.
    pub fn adopt_subscription(&mut self, previous: &mut BLECharacteristic) -> Result<()> {
        let Some(handler) = previous
            .subscription
            .as_ref()
            .map(|subscription| Arc::clone(&subscription.handler))
        else {
            return Ok(());
        };
        self.take_registration()?;
        let token = self
            .characteristic
            .ValueChanged(&value_changed(Arc::clone(&handler)))?;
        self.subscription = Some(Subscription { handler, token });
        previous.take_registration()
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
