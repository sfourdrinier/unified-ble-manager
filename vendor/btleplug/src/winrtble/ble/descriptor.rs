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

use super::super::utils;
use crate::{Result, api::Descriptor};
use std::future::IntoFuture;
use uuid::Uuid;
use windows::{
    Devices::Bluetooth::{
        BluetoothCacheMode,
        GenericAttributeProfile::{GattCommunicationStatus, GattDescriptor},
    },
    Storage::Streams::{DataReader, DataWriter},
};

/// Key of one GATT attribute among its siblings: UUID and ATT handle
/// (UBM patch `winrt-attribute-instances`). Two same-UUID siblings differ by
/// handle, so both are kept and addressed exactly.
pub type AttributeKey = (Uuid, u64);

#[derive(Debug)]
pub struct BLEDescriptor {
    descriptor: GattDescriptor,
    uuid: Uuid,
    handle: u16,
}

impl BLEDescriptor {
    pub fn new(descriptor: GattDescriptor) -> Result<Self> {
        let uuid = utils::to_uuid(&descriptor.Uuid()?);
        let handle = descriptor.AttributeHandle()?;
        Ok(Self {
            descriptor,
            uuid,
            handle,
        })
    }

    pub fn key(&self) -> AttributeKey {
        (self.uuid, u64::from(self.handle))
    }

    pub fn gatt(&self) -> &GattDescriptor {
        &self.descriptor
    }

    pub fn to_descriptor(
        &self,
        service_uuid: Uuid,
        service_instance: u64,
        characteristic_uuid: Uuid,
        characteristic_instance: u64,
    ) -> Descriptor {
        Descriptor {
            uuid: self.uuid,
            instance: u64::from(self.handle),
            service_uuid,
            service_instance,
            characteristic_uuid,
            characteristic_instance,
        }
    }

    pub async fn write_value(descriptor: &GattDescriptor, data: &[u8]) -> Result<()> {
        let writer = DataWriter::new()?;
        writer.WriteBytes(data)?;
        // UBM patch (UBM_PATCHES.md #20): a descriptor write is always an
        // ATT Write Request, so the result-returning call sends the same
        // bytes on the wire and additionally reports the ATT error byte of
        // a `ProtocolError` status. The buffer drops before the await
        // (`IBuffer` is not `Send`), so the future stays `Send`.
        let operation = {
            let buffer = writer.DetachBuffer()?;
            descriptor.WriteValueWithResultAsync(&buffer)?
        };
        let result = operation.into_future().await?;
        let status = result.Status()?;
        if status == GattCommunicationStatus::Success {
            Ok(())
        } else {
            let mut att = None;
            if status == GattCommunicationStatus::ProtocolError {
                att = utils::protocol_att_error(result.ProtocolError());
            }
            Err(utils::gatt_status_error(
                "Gatt descriptor write",
                status,
                att,
            ))
        }
    }

    pub async fn read_value(descriptor: &GattDescriptor) -> Result<Vec<u8>> {
        let result = descriptor
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
            // UBM patch (UBM_PATCHES.md #20): a `ProtocolError` status
            // carries the ATT error byte, so the host can tell a security
            // refusal from any other protocol error.
            let mut att = None;
            if status == GattCommunicationStatus::ProtocolError {
                att = utils::protocol_att_error(result.ProtocolError());
            }
            Err(utils::gatt_status_error(
                "Gatt descriptor read",
                status,
                att,
            ))
        }
    }
}
