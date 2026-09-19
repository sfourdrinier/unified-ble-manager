//! Bluetooth Management (MGMT) packets for the `mgmt-legacy` advertising
//! backend: pure encoding and parsing, compiled and tested on every platform.
//! The socket that carries them is Linux-only (`src/mgmt_socket.rs`).
//!
//! Layouts follow the kernel's `include/net/bluetooth/mgmt.h` (identical in
//! 6.8 and 7.0; all multi-byte fields little-endian, every struct
//! `__packed`):
//!
//! ```c
//! struct mgmt_hdr { __le16 opcode; __le16 index; __le16 len; };
//!
//! #define MGMT_OP_READ_ADV_FEATURES 0x003D        // no parameters
//! struct mgmt_rp_read_adv_features {
//!     __le32 supported_flags; __u8 max_adv_data_len; __u8 max_scan_rsp_len;
//!     __u8 max_instances; __u8 num_instances; __u8 instance[]; };
//!
//! #define MGMT_OP_ADD_ADVERTISING 0x003E
//! struct mgmt_cp_add_advertising {
//!     __u8 instance; __le32 flags; __le16 duration; __le16 timeout;
//!     __u8 adv_data_len; __u8 scan_rsp_len; __u8 data[]; };
//! #define MGMT_ADD_ADVERTISING_SIZE 11
//!
//! #define MGMT_OP_REMOVE_ADVERTISING 0x003F
//! struct mgmt_cp_remove_advertising { __u8 instance; };
//!
//! #define MGMT_EV_CMD_COMPLETE 0x0001  // { __le16 opcode; __u8 status; __u8 data[]; }
//! #define MGMT_EV_CMD_STATUS   0x0002  // { __le16 opcode; __u8 status; }
//! #define MGMT_EV_INDEX_REMOVED 0x0005
//! #define MGMT_EV_ADVERTISING_ADDED   0x0023  // { __u8 instance; }
//! #define MGMT_EV_ADVERTISING_REMOVED 0x0024  // { __u8 instance; }
//! ```
//!
//! Why this command and not its extended successor: bluetoothd registers
//! `LEAdvertisement1` objects with `MGMT_OP_ADD_EXT_ADV_PARAMS` (0x0054) plus
//! `MGMT_OP_ADD_EXT_ADV_DATA` (0x0055). The affected BlueZ releases size the
//! 0x0055 parameters with `sizeof(struct mgmt_cp_add_advertising)` (11) where
//! `struct mgmt_cp_add_ext_adv_data` is 3 bytes, so every request carries 8
//! extra bytes, and kernels that check the length exactly answer `Invalid
//! Parameters (0x0d)`. `MGMT_OP_ADD_ADVERTISING` carries its own correctly
//! sized header and still drives the controller's extended advertising sets.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use crate::advertisement;

/// `struct mgmt_hdr` size.
pub const HEADER_LEN: usize = 6;
/// `MGMT_ADD_ADVERTISING_SIZE`: the fixed part of `mgmt_cp_add_advertising`.
pub const ADD_ADVERTISING_FIXED_LEN: usize = 11;
/// Fixed part of `mgmt_rp_read_adv_features` before `instance[]`.
pub const READ_ADV_FEATURES_FIXED_LEN: usize = 8;

pub const OP_READ_ADV_FEATURES: u16 = 0x003D;
pub const OP_ADD_ADVERTISING: u16 = 0x003E;
pub const OP_REMOVE_ADVERTISING: u16 = 0x003F;

pub const EV_CMD_COMPLETE: u16 = 0x0001;
pub const EV_CMD_STATUS: u16 = 0x0002;
pub const EV_INDEX_REMOVED: u16 = 0x0005;
pub const EV_ADVERTISING_ADDED: u16 = 0x0023;
pub const EV_ADVERTISING_REMOVED: u16 = 0x0024;

/// `MGMT_ADV_FLAG_CONNECTABLE`: connectable undirected advertising.
pub const ADV_FLAG_CONNECTABLE: u32 = 1 << 0;
/// `MGMT_ADV_FLAG_DISCOV`: the kernel writes a general-discoverable Flags AD.
pub const ADV_FLAG_DISCOV: u32 = 1 << 1;
/// `MGMT_ADV_FLAG_LIMITED_DISCOV`: the kernel writes a limited-discoverable Flags AD.
pub const ADV_FLAG_LIMITED_DISCOV: u32 = 1 << 2;
/// `MGMT_ADV_FLAG_MANAGED_FLAGS`: the kernel writes the Flags AD itself.
pub const ADV_FLAG_MANAGED_FLAGS: u32 = 1 << 3;
/// Every flag with which the kernel owns the Flags AD. With any of them set,
/// `tlv_data_is_valid()` (net/bluetooth/mgmt.c) rejects a caller-supplied
/// Flags AD structure as `Invalid Parameters`.
pub const KERNEL_MANAGED_FLAGS_MASK: u32 =
    ADV_FLAG_DISCOV | ADV_FLAG_LIMITED_DISCOV | ADV_FLAG_MANAGED_FLAGS;

pub const STATUS_SUCCESS: u8 = 0x00;
pub const STATUS_PERMISSION_DENIED: u8 = 0x14;

/// AD types (Bluetooth Assigned Numbers, "Common Data Types").
pub const AD_FLAGS: u8 = 0x01;
pub const AD_UUID16_COMPLETE: u8 = 0x03;
pub const AD_NAME_COMPLETE: u8 = 0x09;
pub const AD_MANUFACTURER_DATA: u8 = 0xFF;

/// The Flags AD value a real (LE-only) H10 sends: LE General Discoverable
/// (bit 1) plus BR/EDR Not Supported (bit 2). Written by the sim, not the
/// kernel, so it does not change with the adapter's own BR/EDR setting.
pub const H10_AD_FLAGS: u8 = 0x06;

/// `mgmt_status()` names for the statuses this backend can meet
/// (include/net/bluetooth/mgmt.h, `MGMT_STATUS_*`).
pub fn status_name(status: u8) -> &'static str {
    match status {
        0x00 => "Success",
        0x01 => "Unknown Command",
        0x03 => "Failed",
        0x07 => "No Resources",
        0x0A => "Busy",
        0x0B => "Rejected",
        0x0C => "Not Supported",
        0x0D => "Invalid Parameters",
        0x0F => "Not Powered",
        0x11 => "Invalid Index",
        0x12 => "RFKilled",
        0x14 => "Permission Denied",
        _ => "unlisted status",
    }
}

/// One MGMT command packet: `mgmt_hdr` followed by the parameters.
pub fn command(opcode: u16, index: u16, params: &[u8]) -> Result<Vec<u8>, String> {
    let len = u16::try_from(params.len())
        .map_err(|_| format!("MGMT parameters of {} bytes exceed u16", params.len()))?;
    let mut packet = Vec::with_capacity(HEADER_LEN + params.len());
    packet.extend_from_slice(&opcode.to_le_bytes());
    packet.extend_from_slice(&index.to_le_bytes());
    packet.extend_from_slice(&len.to_le_bytes());
    packet.extend_from_slice(params);
    Ok(packet)
}

/// `mgmt_cp_add_advertising` with its payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddAdvertising {
    pub instance: u8,
    pub flags: u32,
    pub duration: u16,
    pub timeout: u16,
    pub adv_data: Vec<u8>,
    pub scan_rsp: Vec<u8>,
}

impl AddAdvertising {
    /// Parameter bytes, refusing combinations the kernel would reject so the
    /// failure names its cause here instead of a bare `0x0d`.
    pub fn params(&self) -> Result<Vec<u8>, String> {
        if self.instance == 0 {
            return Err("advertising instance 0 means \"all\"; instances start at 1".to_string());
        }
        let adv_len = u8::try_from(self.adv_data.len())
            .map_err(|_| format!("advertising data of {} bytes", self.adv_data.len()))?;
        let scan_len = u8::try_from(self.scan_rsp.len())
            .map_err(|_| format!("scan response of {} bytes", self.scan_rsp.len()))?;
        if self.flags & KERNEL_MANAGED_FLAGS_MASK != 0
            && ad_structures(&self.adv_data)?
                .iter()
                .any(|(ad_type, _)| *ad_type == AD_FLAGS)
        {
            return Err(format!(
                "advertising flags 0x{:08x} let the kernel own the Flags AD, \
                 so the payload may not carry one",
                self.flags
            ));
        }
        ad_structures(&self.scan_rsp)?;
        let mut params = Vec::with_capacity(
            ADD_ADVERTISING_FIXED_LEN + self.adv_data.len() + self.scan_rsp.len(),
        );
        params.push(self.instance);
        params.extend_from_slice(&self.flags.to_le_bytes());
        params.extend_from_slice(&self.duration.to_le_bytes());
        params.extend_from_slice(&self.timeout.to_le_bytes());
        params.push(adv_len);
        params.push(scan_len);
        params.extend_from_slice(&self.adv_data);
        params.extend_from_slice(&self.scan_rsp);
        Ok(params)
    }
}

/// `mgmt_cp_remove_advertising`.
pub fn remove_advertising_params(instance: u8) -> Vec<u8> {
    vec![instance]
}

/// Splits AD bytes into `(type, value)` structures, refusing a structure that
/// runs past the end — the kernel's `tlv_data_is_valid()` rule.
pub fn ad_structures(data: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut structures = Vec::new();
    let mut offset = 0;
    while offset < data.len() {
        let len = usize::from(data[offset]);
        if len == 0 {
            offset += 1;
            continue;
        }
        let end = offset + 1 + len;
        if end > data.len() {
            return Err(format!(
                "AD structure at byte {offset} claims {len} bytes, {} remain",
                data.len() - offset - 1
            ));
        }
        structures.push((data[offset + 1], &data[offset + 2..end]));
        offset = end;
    }
    Ok(structures)
}

fn push_ad(out: &mut Vec<u8>, ad_type: u8, value: &[u8]) -> Result<(), String> {
    let len = u8::try_from(value.len() + 1)
        .map_err(|_| format!("AD type 0x{ad_type:02x} value of {} bytes", value.len()))?;
    out.push(len);
    out.push(ad_type);
    out.extend_from_slice(value);
    Ok(())
}

/// Advertising data in the H10 layout ([`crate::advertisement`]): Flags
/// `02 01 06`, the complete 16-bit UUID list, then manufacturer data when a
/// payload is staged. Refuses anything over the legacy 31-byte budget.
pub fn h10_adv_data(uuids16: &[u16], mfr: Option<(u16, &[u8])>) -> Result<Vec<u8>, String> {
    let mut data = Vec::with_capacity(advertisement::MAX_LEGACY_PAYLOAD);
    push_ad(&mut data, AD_FLAGS, &[H10_AD_FLAGS])?;
    let uuid_bytes: Vec<u8> = uuids16.iter().flat_map(|uuid| uuid.to_le_bytes()).collect();
    push_ad(&mut data, AD_UUID16_COMPLETE, &uuid_bytes)?;
    if let Some((company, payload)) = mfr.filter(|(_, payload)| !payload.is_empty()) {
        let mut value = company.to_le_bytes().to_vec();
        value.extend_from_slice(payload);
        push_ad(&mut data, AD_MANUFACTURER_DATA, &value)?;
    }
    if data.len() > advertisement::MAX_LEGACY_PAYLOAD {
        return Err(format!(
            "advertising data of {} bytes exceeds the {}-byte legacy budget",
            data.len(),
            advertisement::MAX_LEGACY_PAYLOAD
        ));
    }
    Ok(data)
}

/// Scan response in the H10 layout: the complete local name, nothing else.
pub fn h10_scan_rsp(name: &str) -> Result<Vec<u8>, String> {
    let mut data = Vec::with_capacity(advertisement::MAX_LEGACY_PAYLOAD);
    push_ad(&mut data, AD_NAME_COMPLETE, name.as_bytes())?;
    if data.len() > advertisement::MAX_LEGACY_PAYLOAD {
        return Err(format!(
            "scan response of {} bytes exceeds the {}-byte legacy budget (fit the name first)",
            data.len(),
            advertisement::MAX_LEGACY_PAYLOAD
        ));
    }
    Ok(data)
}

/// The complete `MGMT_OP_ADD_ADVERTISING` request for the H10 layout:
/// connectable, Flags written by the sim (so no kernel-managed flag bits),
/// no duration, no timeout.
pub fn h10_add_advertising(
    instance: u8,
    name: &str,
    uuids16: &[u16],
    mfr: Option<(u16, &[u8])>,
) -> Result<AddAdvertising, String> {
    Ok(AddAdvertising {
        instance,
        flags: ADV_FLAG_CONNECTABLE,
        duration: 0,
        timeout: 0,
        adv_data: h10_adv_data(uuids16, mfr)?,
        scan_rsp: h10_scan_rsp(name)?,
    })
}

/// A decoded MGMT event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    CommandComplete {
        index: u16,
        opcode: u16,
        status: u8,
        data: Vec<u8>,
    },
    CommandStatus {
        index: u16,
        opcode: u16,
        status: u8,
    },
    IndexRemoved {
        index: u16,
    },
    AdvertisingAdded {
        index: u16,
        instance: u8,
    },
    AdvertisingRemoved {
        index: u16,
        instance: u8,
    },
    /// Any other event: not addressed to this backend, kept with its code so
    /// a caller can still count or print it.
    Other {
        index: u16,
        code: u16,
    },
}

/// Parses one event packet (one `read()` on the control socket).
pub fn parse_event(packet: &[u8]) -> Result<Event, String> {
    if packet.len() < HEADER_LEN {
        return Err(format!(
            "MGMT event of {} bytes has no header",
            packet.len()
        ));
    }
    let code = u16::from_le_bytes([packet[0], packet[1]]);
    let index = u16::from_le_bytes([packet[2], packet[3]]);
    let len = usize::from(u16::from_le_bytes([packet[4], packet[5]]));
    let params = &packet[HEADER_LEN..];
    if params.len() != len {
        return Err(format!(
            "MGMT event 0x{code:04x} declares {len} parameter bytes, carries {}",
            params.len()
        ));
    }
    let need = |wanted: usize| {
        if params.len() < wanted {
            Err(format!(
                "MGMT event 0x{code:04x} has {} parameter bytes, needs {wanted}",
                params.len()
            ))
        } else {
            Ok(())
        }
    };
    Ok(match code {
        EV_CMD_COMPLETE => {
            need(3)?;
            Event::CommandComplete {
                index,
                opcode: u16::from_le_bytes([params[0], params[1]]),
                status: params[2],
                data: params[3..].to_vec(),
            }
        }
        EV_CMD_STATUS => {
            need(3)?;
            Event::CommandStatus {
                index,
                opcode: u16::from_le_bytes([params[0], params[1]]),
                status: params[2],
            }
        }
        EV_INDEX_REMOVED => Event::IndexRemoved { index },
        EV_ADVERTISING_ADDED => {
            need(1)?;
            Event::AdvertisingAdded {
                index,
                instance: params[0],
            }
        }
        EV_ADVERTISING_REMOVED => {
            need(1)?;
            Event::AdvertisingRemoved {
                index,
                instance: params[0],
            }
        }
        _ => Event::Other { index, code },
    })
}

/// `mgmt_rp_read_adv_features`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvFeatures {
    pub supported_flags: u32,
    pub max_adv_data_len: u8,
    pub max_scan_rsp_len: u8,
    pub max_instances: u8,
    /// Instances the kernel lists. `read_adv_features()` lists only
    /// instances numbered at most the current instance count, so a
    /// higher-numbered instance can exist without appearing here.
    pub instances: Vec<u8>,
}

pub fn parse_adv_features(data: &[u8]) -> Result<AdvFeatures, String> {
    if data.len() < READ_ADV_FEATURES_FIXED_LEN {
        return Err(format!(
            "Read Advertising Features reply of {} bytes, needs {READ_ADV_FEATURES_FIXED_LEN}",
            data.len()
        ));
    }
    let listed = usize::from(data[7]);
    let instances = &data[READ_ADV_FEATURES_FIXED_LEN..];
    if instances.len() != listed {
        return Err(format!(
            "Read Advertising Features lists {listed} instances, carries {}",
            instances.len()
        ));
    }
    Ok(AdvFeatures {
        supported_flags: u32::from_le_bytes([data[0], data[1], data[2], data[3]]),
        max_adv_data_len: data[4],
        max_scan_rsp_len: data[5],
        max_instances: data[6],
        instances: instances.to_vec(),
    })
}

/// Picks an instance that is provably free and that the kernel will keep
/// listing once added.
///
/// The kernel lists every instance numbered at most its instance count `c`.
/// So any number `k <= c` missing from the list is free, and with all `c`
/// numbers listed the instances are exactly `1..=c` and `c + 1` is free.
/// The reply does not carry `c`, only `listed.len() <= c`. A gap in
/// `1..=listed.len()` is at most `c` and unlisted, so it is free. Without a
/// gap the listed set is exactly `1..=listed.len()`; `listed.len() + 1` is
/// then either above `c` with every instance listed, or at most `c` and
/// unlisted — free in both cases. Either way the chosen number is at most the
/// new count once added, so the kernel keeps listing it.
/// Of the provably free numbers the highest is taken, keeping clear of
/// bluetoothd, which allocates its own instance ids lowest-first.
pub fn pick_instance(features: &AdvFeatures) -> Result<u8, String> {
    let listed = features.instances.len();
    let gap = (1..=listed)
        .rev()
        .filter_map(|candidate| u8::try_from(candidate).ok())
        .find(|candidate| !features.instances.contains(candidate));
    let chosen = match gap {
        Some(free) => free,
        None => u8::try_from(listed + 1).map_err(|_| "instance number overflow".to_string())?,
    };
    if chosen > features.max_instances {
        return Err(format!(
            "no free advertising instance: the controller supports {} and the kernel lists {:?} \
             (`btmgmt advinfo`; instances leaked by failed bluetoothd registrations are freed \
             by `sudo systemctl restart bluetooth`)",
            features.max_instances, features.instances
        ));
    }
    Ok(chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NAME: &str = "Polar H10 SIM0001";
    const UUIDS: [u16; 2] = [0x180D, 0xFEEE];

    fn features(max_instances: u8, instances: &[u8]) -> AdvFeatures {
        AdvFeatures {
            supported_flags: 0,
            max_adv_data_len: 251,
            max_scan_rsp_len: 251,
            max_instances,
            instances: instances.to_vec(),
        }
    }

    #[test]
    fn header_is_opcode_index_len_little_endian() {
        assert_eq!(
            command(OP_READ_ADV_FEATURES, 0, &[]).unwrap(),
            vec![0x3D, 0x00, 0x00, 0x00, 0x00, 0x00]
        );
        assert_eq!(
            command(OP_REMOVE_ADVERTISING, 1, &remove_advertising_params(16)).unwrap(),
            vec![0x3F, 0x00, 0x01, 0x00, 0x01, 0x00, 0x10]
        );
    }

    #[test]
    fn h10_advertising_data_golden_bytes() {
        assert_eq!(
            h10_adv_data(&UUIDS, None).unwrap(),
            vec![0x02, 0x01, 0x06, 0x05, 0x03, 0x0D, 0x18, 0xEE, 0xFE]
        );
        assert_eq!(
            h10_adv_data(&UUIDS, None).unwrap().len(),
            advertisement::ADV_FIXED_LEN,
            "same layout the advertisement budget pins"
        );
    }

    #[test]
    fn staged_manufacturer_data_follows_the_uuid_list() {
        let data = h10_adv_data(&UUIDS, Some((0x006B, &[0x33, 0x1C]))).unwrap();
        assert_eq!(
            data,
            vec![
                0x02, 0x01, 0x06, 0x05, 0x03, 0x0D, 0x18, 0xEE, 0xFE, 0x05, 0xFF, 0x6B, 0x00, 0x33,
                0x1C
            ]
        );
        assert_eq!(data.len(), advertisement::adv_len_with_mfr(2));
        assert_eq!(
            h10_adv_data(&UUIDS, Some((0x006B, &[]))).unwrap().len(),
            9,
            "an empty payload stays off the air, as on the bluez path"
        );
    }

    #[test]
    fn oversized_manufacturer_data_is_refused() {
        let payload = [0u8; 19];
        let error = h10_adv_data(&UUIDS, Some((0x006B, &payload))).unwrap_err();
        assert!(error.contains("32 bytes exceeds the 31-byte"), "{error}");
    }

    #[test]
    fn scan_response_carries_the_complete_name() {
        let mut expected = vec![0x12, 0x09];
        expected.extend_from_slice(NAME.as_bytes());
        assert_eq!(h10_scan_rsp(NAME).unwrap(), expected);
        assert_eq!(
            h10_scan_rsp(NAME).unwrap().len(),
            advertisement::advertisement_sizes(NAME).scan_rsp_len
        );
        assert!(h10_scan_rsp(&"P".repeat(advertisement::MAX_NAME_LEN)).is_ok());
        assert!(h10_scan_rsp(&"P".repeat(advertisement::MAX_NAME_LEN + 1)).is_err());
    }

    #[test]
    fn add_advertising_golden_packet() {
        let request = h10_add_advertising(2, NAME, &UUIDS, None).unwrap();
        let packet = command(OP_ADD_ADVERTISING, 0, &request.params().unwrap()).unwrap();
        let mut expected = vec![
            0x3E, 0x00, // opcode MGMT_OP_ADD_ADVERTISING
            0x00, 0x00, // controller index hci0
            0x27, 0x00, // 39 parameter bytes = 11 + 9 + 19
            0x02, // instance
            0x01, 0x00, 0x00, 0x00, // flags = MGMT_ADV_FLAG_CONNECTABLE
            0x00, 0x00, // duration
            0x00, 0x00, // timeout
            0x09, // adv_data_len
            0x13, // scan_rsp_len
            0x02, 0x01, 0x06, 0x05, 0x03, 0x0D, 0x18, 0xEE, 0xFE, 0x12, 0x09,
        ];
        expected.extend_from_slice(NAME.as_bytes());
        assert_eq!(packet, expected);
        assert_eq!(
            packet.len() - HEADER_LEN,
            ADD_ADVERTISING_FIXED_LEN + 9 + 19,
            "the kernel checks data_len == sizeof(*cp) + adv_data_len + scan_rsp_len exactly"
        );
    }

    #[test]
    fn flags_are_connectable_with_the_flags_ad_in_the_payload() {
        let request = h10_add_advertising(1, NAME, &UUIDS, None).unwrap();
        assert_eq!(request.flags, ADV_FLAG_CONNECTABLE);
        assert_eq!(
            request.flags & KERNEL_MANAGED_FLAGS_MASK,
            0,
            "discoverability is the Flags AD the sim writes, never a kernel-managed flag"
        );
        let structures = ad_structures(&request.adv_data).unwrap();
        assert_eq!(structures[0], (AD_FLAGS, &[H10_AD_FLAGS][..]));
        assert!(
            ad_structures(&request.scan_rsp)
                .unwrap()
                .iter()
                .all(|(ad_type, _)| *ad_type != AD_FLAGS),
            "the kernel refuses a Flags AD in a scan response"
        );
    }

    #[test]
    fn kernel_managed_flags_with_a_flags_ad_are_refused_before_the_kernel() {
        for managed in [
            ADV_FLAG_DISCOV,
            ADV_FLAG_LIMITED_DISCOV,
            ADV_FLAG_MANAGED_FLAGS,
        ] {
            let mut request = h10_add_advertising(1, NAME, &UUIDS, None).unwrap();
            request.flags |= managed;
            let error = request.params().unwrap_err();
            assert!(error.contains("own the Flags AD"), "{error}");
        }
    }

    #[test]
    fn instance_zero_is_refused() {
        let request = h10_add_advertising(0, NAME, &UUIDS, None).unwrap();
        assert!(request
            .params()
            .unwrap_err()
            .contains("instances start at 1"));
    }

    #[test]
    fn truncated_ad_structure_is_refused() {
        assert!(ad_structures(&[0x05, 0x03, 0x0D]).is_err());
        assert_eq!(
            ad_structures(&[0x00, 0x02, 0x01, 0x06]).unwrap(),
            vec![(0x01, &[0x06][..])]
        );
    }

    #[test]
    fn parses_command_complete_and_status() {
        // Add Advertising complete for instance 2 on hci0.
        let complete = [0x01, 0x00, 0x00, 0x00, 0x04, 0x00, 0x3E, 0x00, 0x00, 0x02];
        assert_eq!(
            parse_event(&complete).unwrap(),
            Event::CommandComplete {
                index: 0,
                opcode: OP_ADD_ADVERTISING,
                status: STATUS_SUCCESS,
                data: vec![0x02]
            }
        );
        // Read Advertising Features refused on an untrusted socket.
        let status = [0x02, 0x00, 0x00, 0x00, 0x03, 0x00, 0x3D, 0x00, 0x14];
        assert_eq!(
            parse_event(&status).unwrap(),
            Event::CommandStatus {
                index: 0,
                opcode: OP_READ_ADV_FEATURES,
                status: STATUS_PERMISSION_DENIED
            }
        );
        assert_eq!(status_name(STATUS_PERMISSION_DENIED), "Permission Denied");
        assert_eq!(status_name(0x0D), "Invalid Parameters");
    }

    #[test]
    fn parses_advertising_lifecycle_events() {
        assert_eq!(
            parse_event(&[0x24, 0x00, 0x00, 0x00, 0x01, 0x00, 0x02]).unwrap(),
            Event::AdvertisingRemoved {
                index: 0,
                instance: 2
            }
        );
        assert_eq!(
            parse_event(&[0x23, 0x00, 0x01, 0x00, 0x01, 0x00, 0x05]).unwrap(),
            Event::AdvertisingAdded {
                index: 1,
                instance: 5
            }
        );
        assert_eq!(
            parse_event(&[0x05, 0x00, 0x00, 0x00, 0x00, 0x00]).unwrap(),
            Event::IndexRemoved { index: 0 }
        );
        assert_eq!(
            parse_event(&[0x12, 0x00, 0x00, 0x00, 0x00, 0x00]).unwrap(),
            Event::Other {
                index: 0,
                code: 0x12
            }
        );
    }

    #[test]
    fn malformed_events_are_errors_not_guesses() {
        assert!(parse_event(&[0x01, 0x00, 0x00]).is_err());
        assert!(parse_event(&[0x01, 0x00, 0x00, 0x00, 0x05, 0x00, 0x3E]).is_err());
        assert!(parse_event(&[0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x3E]).is_err());
    }

    #[test]
    fn parses_read_adv_features_reply() {
        // As on lx5090wifi: flags 0x3ff, 251/251, 16 sets, one listed (1).
        let reply = [0xFF, 0x03, 0x00, 0x00, 0xFB, 0xFB, 0x10, 0x01, 0x01];
        assert_eq!(
            parse_adv_features(&reply).unwrap(),
            AdvFeatures {
                supported_flags: 0x3FF,
                max_adv_data_len: 251,
                max_scan_rsp_len: 251,
                max_instances: 16,
                instances: vec![1],
            }
        );
        assert!(
            parse_adv_features(&reply[..8]).is_err(),
            "count without the list"
        );
        assert!(parse_adv_features(&reply[..7]).is_err());
    }

    #[test]
    fn picks_the_highest_provably_free_instance() {
        assert_eq!(pick_instance(&features(16, &[])).unwrap(), 1);
        assert_eq!(pick_instance(&features(16, &[1])).unwrap(), 2);
        assert_eq!(pick_instance(&features(16, &[1, 2, 3])).unwrap(), 4);
        // A gap below the listed count is free and stays listed once added.
        assert_eq!(pick_instance(&features(16, &[1, 3, 4])).unwrap(), 2);
        assert_eq!(pick_instance(&features(16, &[3, 4])).unwrap(), 2);
        assert_eq!(pick_instance(&features(10, &[2, 4, 5])).unwrap(), 3);
    }

    #[test]
    fn full_controller_is_refused_loudly() {
        let error = pick_instance(&features(2, &[1, 2])).unwrap_err();
        assert!(error.contains("no free advertising instance"), "{error}");
        assert!(error.contains("[1, 2]"), "{error}");
    }
}
