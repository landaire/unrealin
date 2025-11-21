use byteorder::ReadBytesExt;
use tracing::{Level, span};

use crate::{
    object::{DeserializeUnrealObject, RcUnrealObject},
    reader::UnrealReadExt,
};

#[derive(Clone, Debug, Default)]
pub struct FDependency {
    class: Option<RcUnrealObject>,
    deep: bool,
    script_text_crc: u32,
}

impl DeserializeUnrealObject for FDependency {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut crate::runtime::UnrealRuntime,
        linker: &crate::de::RcLinker,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: crate::reader::LinRead,
    {
        let span = span!(Level::DEBUG, "fdependency");
        let _enter = span.enter();

        self.class = reader.read_object::<E>(runtime, linker)?;
        self.deep = reader.read_u32::<E>()? != 0;
        self.script_text_crc = reader.read_u32::<E>()?;

        Ok(())
    }
}
