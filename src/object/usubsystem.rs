use std::io;

use byteorder::ByteOrder;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

/// Base class for engine subsystems
#[derive(Debug, Default)]
pub struct Subsystem {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for Subsystem {
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

        // USubsystem is an abstract base class with no serialized data

        Ok(())
    }
}
