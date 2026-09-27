use crate::gatt::{
    characteristic::Characteristic,
    descriptor::Descriptor,
    properties::{AttributePermission, CharacteristicProperty},
};
use objc2::{rc::Retained, runtime::AnyObject, ClassType};
use objc2_core_bluetooth::{
    CBAttributePermissions, CBCharacteristicProperties, CBDescriptor, CBMutableCharacteristic,
    CBMutableDescriptor,
};
use objc2_foundation::{NSArray, NSData};

use super::mac_extensions::uuid_to_cbuuid;

pub fn parse_characteristic(characteristic: &Characteristic) -> Retained<CBMutableCharacteristic> {
    unsafe {
        let properties = characteristic
            .properties
            .iter()
            .fold(CBCharacteristicProperties::empty(), |acc, property| {
                acc | property.clone().to_cb_property()
            });

        let permissions = characteristic
            .permissions
            .iter()
            .fold(CBAttributePermissions::empty(), |acc, permission| {
                acc | permission.clone().to_attribute_permission()
            });

        let value_data = characteristic
            .value
            .as_ref()
            // CoreBluetooth cached values are immutable read-only attributes.
            // Dynamic read/write/notify values must use the existing delegate
            // callbacks, not a non-nil initializer that invalidates publication.
            .filter(|_| {
                properties == CBCharacteristicProperties::CBCharacteristicPropertyRead
                    && permissions == CBAttributePermissions::Readable
            })
            .map(|value| NSData::from_vec(value.clone()));

        let mutable_char = CBMutableCharacteristic::initWithType_properties_value_permissions(
            CBMutableCharacteristic::alloc(),
            &uuid_to_cbuuid(characteristic.uuid),
            properties,
            value_data.as_ref().map(|data| data as &NSData),
            permissions,
        );

        let descriptors: Retained<NSArray<CBDescriptor>> = NSArray::from_vec(
            characteristic
                .descriptors
                .iter()
                .map(|desc| parse_descriptor(desc))
                .collect(),
        );

        mutable_char.setDescriptors(Some(&descriptors));
        if !descriptors.is_empty() {
            log::debug!("DescriptorAdded");
        }
        return mutable_char;
    }
}

pub fn parse_descriptor(descriptor: &Descriptor) -> Retained<CBDescriptor> {
    unsafe {
        let value_data = descriptor
            .value
            .as_ref()
            .map(|value| NSData::from_vec(value.clone()));

        return Retained::into_super(CBMutableDescriptor::initWithType_value(
            CBMutableDescriptor::alloc(),
            &uuid_to_cbuuid(descriptor.uuid),
            value_data.as_ref().map(|data| data as &AnyObject),
        ));
    }
}

impl CharacteristicProperty {
    fn to_cb_property(self) -> CBCharacteristicProperties {
        return match self {
            CharacteristicProperty::Broadcast => {
                CBCharacteristicProperties::CBCharacteristicPropertyBroadcast
            }
            CharacteristicProperty::Read => {
                CBCharacteristicProperties::CBCharacteristicPropertyRead
            }
            CharacteristicProperty::WriteWithoutResponse => {
                CBCharacteristicProperties::CBCharacteristicPropertyWriteWithoutResponse
            }
            CharacteristicProperty::Write => {
                CBCharacteristicProperties::CBCharacteristicPropertyWrite
            }
            CharacteristicProperty::Notify => {
                CBCharacteristicProperties::CBCharacteristicPropertyNotify
            }
            CharacteristicProperty::NotifyEncryptionRequired => {
                CBCharacteristicProperties::CBCharacteristicPropertyNotifyEncryptionRequired
            }
            CharacteristicProperty::Indicate => {
                CBCharacteristicProperties::CBCharacteristicPropertyIndicate
            }
            CharacteristicProperty::IndicateEncryptionRequired => {
                CBCharacteristicProperties::CBCharacteristicPropertyIndicateEncryptionRequired
            }
            CharacteristicProperty::AuthenticatedSignedWrites => {
                CBCharacteristicProperties::CBCharacteristicPropertyAuthenticatedSignedWrites
            }
            CharacteristicProperty::ExtendedProperties => {
                CBCharacteristicProperties::CBCharacteristicPropertyExtendedProperties
            }
        };
    }
}

impl AttributePermission {
    fn to_attribute_permission(self) -> CBAttributePermissions {
        return match self {
            AttributePermission::Readable => CBAttributePermissions::Readable,
            AttributePermission::Writeable => CBAttributePermissions::Writeable,
            AttributePermission::ReadEncryptionRequired => {
                CBAttributePermissions::ReadEncryptionRequired
            }
            AttributePermission::WriteEncryptionRequired => {
                CBAttributePermissions::WriteEncryptionRequired
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn dynamic_values_preserve_properties_without_corebluetooth_cached_values() {
        for properties in [
            vec![CharacteristicProperty::Read, CharacteristicProperty::Notify],
            vec![
                CharacteristicProperty::Read,
                CharacteristicProperty::Write,
                CharacteristicProperty::Indicate,
            ],
        ] {
            let source = Characteristic {
                uuid: Uuid::from_u128(0x2a19),
                properties,
                permissions: vec![
                    AttributePermission::Readable,
                    AttributePermission::Writeable,
                ],
                value: Some(vec![72]),
                descriptors: Vec::new(),
            };
            // Constructs the native attribute only: no manager, adapter or advertising.
            let characteristic = parse_characteristic(&source);
            unsafe {
                assert!(characteristic.value().is_none());
                let expected = source
                    .properties
                    .iter()
                    .fold(CBCharacteristicProperties::empty(), |all, property| {
                        all | property.clone().to_cb_property()
                    });
                assert_eq!(characteristic.properties(), expected);
                assert_eq!(
                    characteristic.permissions(),
                    CBAttributePermissions::Readable | CBAttributePermissions::Writeable
                );
            }
        }
    }

    #[test]
    fn immutable_read_only_value_remains_cached() {
        let source = Characteristic {
            uuid: Uuid::from_u128(0x2a29),
            properties: vec![CharacteristicProperty::Read],
            permissions: vec![AttributePermission::Readable],
            value: Some(vec![72]),
            descriptors: Vec::new(),
        };
        let characteristic = parse_characteristic(&source);
        unsafe {
            assert_eq!(characteristic.value().unwrap().length(), 1);
        }
    }
}
