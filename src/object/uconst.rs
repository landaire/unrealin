use std::io;

use byteorder::ByteOrder;
use tracing::{Level, debug, span};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, ufield::Field},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

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