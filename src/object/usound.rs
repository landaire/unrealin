use std::io;

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, debug, span};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirror of SC's `USound::Serialize` (Engine_demo `0x1030273e` tail-calls
/// `0x103ce3e0`):
///   - `Super::Serialize` (UObject tag loop).
///   - if `LicenseeVer < 0xA && IsLoading`: legacy `FString DareEvent` then,
///     if it isn't literally `"DareEvent"`, `operator<<(FArchive, FSoundData)`
///     inline. SC's licensee version is `> 0x1A`, so this entire block is
///     skipped.
///   - if `LicenseeVer > 3`: raw 4-byte `FArchive::Serialize` at `+0x30`
///     (the FName slot for the audio asset name).
///   - if `LicenseeVer >= 7`: raw 4-byte `FArchive::Serialize` at `+0x38`
///     followed by `operator<<(FArchive, FString)` at `+0x3c` (the lipsynch
///     filename).
///
/// For SC's `Play_FisherSlideTube` (10 bytes total) the layout decodes as
/// `1 (None tag) + 4 + 4 + 1 (empty FString) = 10` bytes.
#[derive(Default, Debug)]
pub struct Sound {
    pub parent_object: Object,
    pub name_index: u32,
    pub lipsynch_flag: u32,
    pub lipsynch_filename: String,
}

impl DeserializeUnrealObject for Sound {
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
        let span = span!(Level::DEBUG, "deserialize_sound");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let licensee_version = linker.borrow().licensee_version();

        if licensee_version > 3 {
            self.name_index = reader.read_u32::<E>()?;
        }

        if licensee_version >= 7 {
            self.lipsynch_flag = reader.read_u32::<E>()?;
            self.lipsynch_filename = reader.read_string()?;
        }

        debug!(
            "Sound: name_index={:#x} lipsynch_flag={:#x} lipsynch_filename={:?}",
            self.name_index, self.lipsynch_flag, self.lipsynch_filename
        );
        Ok(())
    }
}
