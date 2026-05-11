use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uprimitive::Primitive;
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

#[derive(Debug, Default)]
pub struct Cylinder {
    pub parent_object: Primitive,
    pub radius: f32,
    pub height: f32,
}

impl DeserializeUnrealObject for Cylinder {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        self.parent_object
            .deserialize::<E, R>(runtime, linker, reader)?;

        // Read cylinder-specific properties
        self.radius = reader.read_f32::<E>()?;
        self.height = reader.read_f32::<E>()?;

        Ok(())
    }
}
