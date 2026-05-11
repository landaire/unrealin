use tracing::Level;
use tracing::span;

use crate::object::DeserializeUnrealObject;
use crate::object::uobject::Object;

#[derive(Default, Debug)]
pub struct AudioSubsystem {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for AudioSubsystem {
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
        let span = span!(Level::DEBUG, "audio_subsystem");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)
    }
}
