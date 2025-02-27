//! GPT labeling for Nexus devices. The primary partition
//! (/dev/x1) will be used for meta data during, rebuild. The second
//! partition contains the file system.
//!
//! The nexus will adjust internal data structures to offset the IO to the
//! right partition. put differently, when connecting to this device via
//! NVMF or iSCSI it will show up as device with just one partition.
//!
//! When the nexus is removed from the data path and other initiations are
//! used, the data is still accessible and thus removes us has a hard
//! dependency in the data path.
//!
//! # Example:
//!
//! ```bash
//! $ rm /code/disk1.img; truncate -s 1GiB /code/disk1.img
//! $ mctl create gpt  -r  aio:////code/disk1.img?blk_size=512 -s 1GiB -b
//! $ sgdisk -p /code/disk1.img
//! Found valid GPT with corrupt MBR; using GPT and will write new
//! protective MBR on save.
//! Disk /code//disk1.img: 2097152 sectors, 1024.0 MiB
//! Sector size (logical): 512 bytes
//! Disk identifier (GUID): EAB49A2F-EFEA-45E6-9A1B-61FECE3426DD
//! Partition table holds up to 128 entries
//! Main partition table begins at sector 2 and ends at sector 33
//! First usable sector is 2048, last usable sector is 2097118
//! Partitions will be aligned on 2048-sector boundaries
//! Total free space is 0 sectors (0 bytes)
//!
//! Number  Start (sector)    End (sector)  Size       Code  Name
//!  1            2048           10239   4.0 MiB     FFFF  MayaMeta
//!  2           10240         2097118   1019.0 MiB  FFFF  MayaData
//! ```
//!
//! Notice how two partitions have been created when accessing the disk
//! when shared by the nexus:
//!
//! ```bash
//! $ mctl share gpt
//! "/dev/nbd0"
//!
//! TODO: also note how it complains about a MBR
//!
//! $ lsblk
//! NAME    MAJ:MIN RM  SIZE RO TYPE MOUNTPOINT
//! sda       8:0    0   50G  0 disk
//! ├─sda1    8:1    0 41.5G  0 part /
//! ├─sda2    8:2    0    7M  0 part [SWAP]
//! └─sda3    8:3    0  511M  0 part /boot
//! sr0      11:0    1 1024M  0 rom
//! nbd0     43:0    0 1019M  0 disk
//! nvme0n1 259:0    0  200G  0 disk /code
//!
//! The nbd0 zero device does not show the partitions
//! ```
use crate::bdev::nexus::Error;
use bincode::{deserialize_from, serialize};
use crc::{crc32, Hasher32};
use serde::{
    de::{Deserialize, Deserializer, SeqAccess, Unexpected, Visitor},
    ser::{Serialize, SerializeTuple, Serializer},
};
use std::{
    fmt::{self, Display},
    io::Cursor,
};
use uuid::{self, parser};

#[derive(Debug, Deserialize, PartialEq, Default, Serialize, Clone, Copy)]

/// based on RFC4122
pub struct GptGuid {
    pub time_low: u32,
    pub time_mid: u16,
    pub time_high: u16,
    pub node: [u8; 8],
}
impl std::str::FromStr for GptGuid {
    type Err = parser::ParseError;

    fn from_str(uuid: &str) -> Result<Self, Self::Err> {
        let fields = uuid::Uuid::from_str(uuid)?;
        let fields = fields.as_fields();

        Ok(GptGuid {
            time_low: fields.0,
            time_mid: fields.1,
            time_high: fields.2,
            node: *fields.3,
        })
    }
}

impl std::fmt::Display for GptGuid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{}",
            uuid::Uuid::from_fields(
                self.time_low,
                self.time_mid,
                self.time_high,
                &self.node,
            )
            .unwrap()
            .to_string()
        )
    }
}

impl GptGuid {
    pub(crate) fn new_random() -> Self {
        let fields = uuid::Uuid::new_v4();
        let fields = fields.as_fields();
        GptGuid {
            time_low: fields.0,
            time_mid: fields.1,
            time_high: fields.2,
            node: *fields.3,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Default, Serialize, Copy, Clone)]
pub struct GPTHeader {
    /// GPT signature (must be "EFI PART").
    pub signature: [u8; 8],
    /// 00 00 01 00 up til version 2.17
    pub revision: [u8; 4],
    /// GPT header size (92 bytes)
    pub header_size: u32,
    /// CRC32  of the header.
    pub self_checksum: u32,
    pub reserved: [u8; 4],
    /// primary lba where the header is located
    pub lba_self: u64,
    /// alternative lba where the header is located (backup)
    pub lba_alt: u64,
    /// first usable lba
    pub lba_start: u64,
    /// last usable lba
    pub lba_end: u64,
    /// 16 bytes representing the GUID of the GPT.
    pub guid: GptGuid,
    /// lba of where to find the partition table
    pub lba_table: u64,
    /// number of partitions, most tools set this to 128
    pub num_entries: u32,
    /// Size of element
    pub entry_size: u32,
    /// CRC32 checksum of the partition array.
    pub table_crc: u32,
}

impl GPTHeader {
    /// converts a slice into a gpt header and verifies the validity of the data
    pub fn from_slice(slice: &[u8]) -> Result<GPTHeader, Error> {
        let mut reader = Cursor::new(slice);
        let mut gpt: GPTHeader = deserialize_from(&mut reader).unwrap();

        if gpt.header_size != 92
            || gpt.signature != [0x45, 0x46, 0x49, 0x20, 0x50, 0x41, 0x52, 0x54]
            || gpt.revision != [0x00, 0x00, 0x01, 0x00]
        {
            return Err(Error::Invalid);
        }

        let crc = gpt.self_checksum;
        gpt.self_checksum = 0;
        gpt.self_checksum = crc32::checksum_ieee(&serialize(&gpt).unwrap());

        if gpt.self_checksum != crc {
            info!("GPT label crc mismatch");
            return Err(Error::Invalid);
        }

        if gpt.lba_self > gpt.lba_alt {
            std::mem::swap(&mut gpt.lba_self, &mut gpt.lba_alt)
        }

        Ok(gpt)
    }

    /// checksum the header with the checksum field itself set 0
    pub fn checksum(&mut self) -> u32 {
        self.self_checksum = 0;
        self.self_checksum = crc32::checksum_ieee(&serialize(&self).unwrap());
        self.self_checksum
    }

    pub fn new(blk_size: u32, num_blocks: u64, guid: uuid::Uuid) -> Self {
        let fields = guid.as_fields();
        GPTHeader {
            signature: [0x45, 0x46, 0x49, 0x20, 0x50, 0x41, 0x52, 0x54],
            revision: [0x00, 0x00, 0x01, 0x00],
            header_size: 92,
            self_checksum: 0,
            reserved: [0; 4],
            lba_self: 1,
            lba_alt: num_blocks - 1,
            lba_start: u64::from((1 << 20) / blk_size),
            lba_end: ((num_blocks - 1) - u64::from((1 << 14) / blk_size)) - 1,
            guid: GptGuid {
                time_low: fields.0,
                time_mid: fields.1,
                time_high: fields.2,
                node: *fields.3,
            },
            lba_table: 2,
            num_entries: 2,
            entry_size: 128,
            table_crc: 0,
        }
    }

    pub fn to_backup(&self) -> Self {
        let mut secondary = *self;
        secondary.lba_self = self.lba_alt;
        secondary.lba_alt = self.lba_self;
        secondary.lba_table = self.lba_end + 1;
        secondary
    }
}

#[derive(Debug, Default, PartialEq, Deserialize, Serialize, Clone)]
pub struct GptEntry {
    /// GUID type, some of them are assigned/reserved for example to Linux
    pub ent_type: GptGuid,
    /// entry GUID, can be anything typically random
    pub ent_guid: GptGuid,
    /// start lba for this entry
    pub ent_start: u64,
    /// end lba for this entry
    pub ent_end: u64,
    /// entry attributes, according to do the docs bit 0 MUST be zero
    pub ent_attr: u64,
    /// utf16 name of the partition entry, do not confuse this fs labels!
    pub ent_name: GptName,
}

impl GptEntry {
    /// converts a slice into a partition array
    pub fn from_slice(
        slice: &[u8],
        parts: u32,
    ) -> Result<Vec<GptEntry>, Error> {
        let mut reader = Cursor::new(slice);
        let mut part_vec = Vec::new();
        // TODO 128 should be passed in as a argument
        for _ in 0 .. parts {
            part_vec.push(deserialize_from(&mut reader)?);
        }
        Ok(part_vec)
    }

    /// calculate the checksum over the partitions table
    pub fn checksum(parts: &[GptEntry]) -> u32 {
        let mut digest = crc32::Digest::new(crc32::IEEE);
        for p in parts {
            digest.write(&serialize(p).unwrap());
        }
        digest.sum32()
    }
}

#[derive(Debug, PartialEq, Serialize, Clone)]
/// The nexus label is standard GPT label (such that you can use it without us
/// in the data path) The only thing that is really specific to us is the
/// ent_type GUID if we see that attached to a partition, we assume the data in
/// that partition is ours. In the data we will have more magic markers to
/// confirm the assumption but this is step one.
pub struct NexusLabel {
    /// the main GPT header
    pub primary: GPTHeader,
    /// Vector of GPT entries where the first element is considered to be ours
    pub partitions: Vec<GptEntry>,
}

impl NexusLabel {
    /// returns the offset to the first data segment
    pub(crate) fn offset(&self) -> u64 {
        self.partitions[1].ent_start
    }

    /// returns the number of total blocks in this segment
    pub(crate) fn num_blocks(&self) -> u64 {
        self.partitions[1].ent_end - self.partitions[1].ent_start
    }
}

impl Display for NexusLabel {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        writeln!(f, "GUID: {}", self.primary.guid.to_string())?;
        writeln!(f, "\tHeader crc32 {}", self.primary.self_checksum)?;
        writeln!(f, "\tPartition table crc32 {}", self.primary.table_crc)?;

        for i in 0 .. self.partitions.len() {
            writeln!(f, "\tPartition number {}", i)?;
            writeln!(f, "\tGUID: {}", self.partitions[i].ent_guid.to_string())?;
            writeln!(
                f,
                "\tType GUID: {}",
                self.partitions[i].ent_type.to_string()
            )?;
            writeln!(
                f,
                "\tLogical block start: {}, end: {}",
                self.partitions[i].ent_start, self.partitions[i].ent_end
            )?;
        }

        Ok(())
    }
}

// for arrays bigger then 32 elements, things start to get unimplemented
// in terms of derive and what not. So we create a struct with a string
// and tell serde how to use it during (de)serializing

struct GpEntryNameVisitor;

impl<'de> Deserialize<'de> for GptName {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_tuple_struct("GptName", 36, GpEntryNameVisitor)
    }
}

impl Serialize for GptName {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // we cant use serialize_type_struct here as we want exactly 72 bytes

        let mut s = serializer.serialize_tuple(36)?;
        let mut out: Vec<u16> = vec![0; 36];
        for (i, o) in self.name.encode_utf16().zip(out.iter_mut()) {
            *o = i;
        }

        out.iter().for_each(|e| s.serialize_element(&e).unwrap());
        s.end()
    }
}
impl<'de> Visitor<'de> for GpEntryNameVisitor {
    type Value = GptName;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("Invalid GPT partition name")
    }

    fn visit_seq<A>(self, mut seq: A) -> std::result::Result<GptName, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut out = Vec::new();
        let mut end = false;
        loop {
            match seq.next_element()? {
                Some(0) => {
                    end = true;
                }
                Some(e) if !end => out.push(e),
                _ => break,
            }
        }

        if end {
            Ok(GptName {
                name: String::from_utf16_lossy(&out),
            })
        } else {
            Err(serde::de::Error::invalid_value(Unexpected::Seq, &self))
        }
    }
}

#[derive(Debug, PartialEq, Default, Clone)]
pub struct GptName {
    pub name: String,
}

impl GptName {
    pub fn as_str(&self) -> &str {
        &self.name
    }
}
