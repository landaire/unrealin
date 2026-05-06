use std::io;

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, debug, span};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirror of SC's `UPalette::Serialize` (Engine_demo 0x10305123,
/// 0x104fbf90):
///   - `Super::Serialize` (UObject, tagged props)
///   - `Ar << Colors` where `Colors` is `TArray<FColor>` and each
///     `FColor` is `(B, G, R, A)` packed as 4 bytes.
///   - For files older than version 66, the alpha byte is patched to
///     `0xFF` post-load (memory-only fixup, no extra I/O).
#[derive(Default, Debug)]
pub struct Palette {
    pub parent_object: Object,
    pub colors: Vec<[u8; 4]>,
}

impl DeserializeUnrealObject for Palette {
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
        let span = span!(Level::DEBUG, "deserialize_palette");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let count = reader.read_packed_int()?;
        assert!(count >= 0, "negative palette color count");
        self.colors = (0..count)
            .map(|_| -> io::Result<[u8; 4]> {
                let mut buf = [0u8; 4];
                reader.cheat(&mut buf)?;
                Ok(buf)
            })
            .collect::<io::Result<Vec<_>>>()?;

        debug!("palette: {} colors", self.colors.len());
        Ok(())
    }
}
