use std::io;

use byteorder::ByteOrder;
use tracing::Level;
use tracing::debug;
use tracing::span;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::ufield::Field;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

#[derive(Debug, Default)]
pub struct Const {
    pub(crate) parent_object: Field,
    pub value: String,
}

impl DeserializeUnrealObject for Const {
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
        let span = span!(Level::DEBUG, "deserialize_const");
        let _enter = span.enter();

        // Deserialize parent Field
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        debug!("deserializing value");
        self.value = reader.read_string()?;
        debug!("Const value: {}", self.value);

        Ok(())
    }
}
