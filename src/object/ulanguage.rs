use tracing::{Level, span};

use crate::object::{DeserializeUnrealObject, uobject::Object};

#[derive(Default, Debug)]
pub struct Language {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for Language {
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
        let span = span!(Level::DEBUG, "language");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)
    }
}
