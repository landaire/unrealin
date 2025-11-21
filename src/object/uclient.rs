use std::io;

use byteorder::ByteOrder;

use crate::de::RcLinker;
use crate::object::{DeserializeUnrealObject, uobject::Object};
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

/// Client class responsible for managing viewports
#[derive(Debug, Default)]
pub struct Client {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for Client {
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
        self.parent_object.deserialize::<E, R>(runtime, linker, reader)?;
        
        // UClient appears to be mostly handled by the engine and doesn't have
        // serialized fields in the package data
        
        Ok(())
    }
}