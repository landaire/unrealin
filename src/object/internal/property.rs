use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, debug, span, trace};

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::internal::fname::FName;
use crate::reader::{LinRead, UnrealReadExt};
use crate::runtime::UnrealRuntime;

#[derive(Default, Debug)]
pub struct PropertyTag {
    pub name: FName,
    pub info: u8,
    pub property_type: u8,
    pub item_name: FName,
    pub size: u32,
    pub array_index: u32,
}

impl PropertyTag {
    pub fn property_type_byte(&self) -> u8 {
        self.property_type
    }
}

const NAME_BOOL_PROPERTY: u8 = 3;
const NAME_STRUCT_PROPERTY: u8 = 10;

impl DeserializeUnrealObject for PropertyTag {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_property_tag");
        let _enter = span.enter();

        debug!("Deserializing name");
        self.name.deserialize::<E, _>(runtime, linker, reader)?;
        if self.name.is_none(&linker.borrow()) {
            trace!("Name is none");
            return Ok(());
        }

        self.info = reader.read_u8()?;
        self.property_type = self.info & 0x0f;

        if self.property_type == NAME_STRUCT_PROPERTY {
            self.item_name.deserialize::<E, _>(runtime, linker, reader)?;
        }

        self.size = match self.info & 0x70 {
            0x00 => 1,
            0x10 => 2,
            0x20 => 4,
            0x30 => 12,
            0x40 => 16,
            0x50 => reader.read_u8()? as u32,
            0x60 => reader.read_u16::<E>()? as u32,
            0x70 => reader.read_u32::<E>()?,
            _ => unreachable!("info size bits exhausted"),
        };

        if (self.info & 0x80) != 0 && self.property_type != NAME_BOOL_PROPERTY {
            let b = reader.read_u8()?;
            if (b & 0x80) == 0 {
                self.array_index = b as u32;
            } else if (b & 0xc0) == 0x80 {
                let c = reader.read_u8()?;
                self.array_index = (((b & 0x7f) as u32) << 8) | (c as u32);
            } else {
                let c = reader.read_u8()?;
                let d = reader.read_u8()?;
                let e = reader.read_u8()?;
                self.array_index = (((b & 0x3f) as u32) << 24)
                    | ((c as u32) << 16)
                    | ((d as u32) << 8)
                    | (e as u32);
            }
        } else {
            self.array_index = 0;
        }

        Ok(())
    }
}
