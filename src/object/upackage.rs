use std::io;

use byteorder::ByteOrder;
use tracing::Level;
use tracing::span;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

/// `UPackage::Serialize` (Core_retail `0x10102d92`) tail-calls
/// `UObject::Serialize`. The export body is just the `None` tag from the
/// parent tagged-property loop (1 byte for sub-package placeholders like
/// `2_2_1_Kalinatek_tex.Fumoire`).
#[derive(Default, Debug)]
pub struct Package {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for Package {
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
        let span = span!(Level::DEBUG, "deserialize_package");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)
    }
}
