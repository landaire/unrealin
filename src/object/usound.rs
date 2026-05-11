use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;
use tracing::Level;
use tracing::debug;
use tracing::span;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirror of SC's `USound::Serialize` (`splintercell.xbe sub_71c80`,
/// equivalent to Engine_demo `0x1030273e -> 0x103ce3e0` plus an SC-specific
/// inline lipsynch loader):
///   - `Super::Serialize` (UObject tag loop).
///   - if `LicenseeVer < 0xA && IsLoading`: legacy `FString DareEvent` then,
///     if it isn't literally `"DareEvent"`, `operator<<(FArchive, FSoundData)`
///     inline. SC's licensee version is `> 0x1A`, so this entire block is
///     skipped.
///   - if `LicenseeVer > 3`: raw 4-byte `FArchive::Serialize` at `+0x30`
///     (the FName slot for the audio asset name).
///   - if `LicenseeVer >= 7`: raw 4-byte `FArchive::Serialize` at `+0x38`
///     (the lipsynch FName slot -- non-zero when lipsynch data should load)
///     followed by `operator<<(FArchive, FString)` at `+0x3c` (the lipsynch
///     filename, e.g. `\S0_0_Voice\00_25_01.bin`).
///   - SC-specific tail: when `IsLoading && +0x38 != 0`, `sub_71c80` rebuilds
///     the path as `..\lipsynch\<filename>`, opens the resulting `.bin` via
///     the engine's `FindFile` mechanism, allocates a buffer of the file's
///     `TotalSize`, and reads the entire file body inline with one
///     `Ar.Serialize(buf, TotalSize)` call. We mirror this by suffix-matching
///     the body filename against the LIN file_table and consuming the
///     matched entry's `len` bytes from the source.
///
/// For SC's `Play_FisherSlideTube` (10 bytes total) the layout decodes as
/// `1 (None tag) + 4 + 4 + 1 (empty FString) = 10` bytes; lipsynch_flag is
/// 0 so no inline load fires.
#[derive(Default, Debug)]
pub struct Sound {
    pub parent_object: Object,
    pub name_index: u32,
    pub lipsynch_flag: u32,
    pub lipsynch_filename: String,
    /// Raw bytes of the inline lipsynch body when one was loaded. Empty when
    /// `lipsynch_flag == 0` or the file_table lookup failed (engine bails
    /// out the same way on `FindFile` open failure).
    pub lipsynch_data: Vec<u8>,
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

            // SC-specific lipsynch inline-load: when the lipsynch FName slot
            // is non-zero the engine opens `..\lipsynch\<filename>` and reads
            // the whole body via one Ar.Serialize call, advancing the linker's
            // archive position past serial_size. The trace records this as a
            // single Read of `TotalSize` bytes immediately after the FString
            // bytes; mirroring it keeps the source cursor aligned with the
            // engine without resorting to draining trace ops.
            if self.lipsynch_flag != 0 && !self.lipsynch_filename.is_empty()
                && let Some(file_size) = runtime.find_file_by_suffix(&self.lipsynch_filename) {
                    let mut buf = vec![0u8; file_size as usize];
                    // The engine reads via a SEPARATE FArchive (the lipsynch
                    // loader) sharing the same FFile, so the source cursor
                    // advances but the outer linker's logical position
                    // doesn't. `read_aliased` matches that: pops the trace
                    // Read op, advances source bytes, leaves self.pos.
                    reader.read_aliased(&mut buf)?;
                    self.lipsynch_data = buf;
                    debug!(
                        "USound: inline-loaded lipsynch {:?} ({:#X} bytes)",
                        self.lipsynch_filename, file_size
                    );
                }
        }

        debug!(
            "Sound: name_index={:#x} lipsynch_flag={:#x} lipsynch_filename={:?}",
            self.name_index, self.lipsynch_flag, self.lipsynch_filename
        );
        Ok(())
    }
}
