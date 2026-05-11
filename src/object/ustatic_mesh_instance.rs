use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirrors `UStaticMeshInstance::Serialize` (Engine_demo `0x10305dcb`
/// tail-calls `sub_104d6ba0`). UStaticMeshInstance derives from
/// UMeshInstance, which has no Serialize override, so the parent path
/// is `UObject::Serialize` directly.
///
/// SC's archive constants (`Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `GIsSavegame=0`):
///
/// 1. `UObject::Serialize` (tagged-property loop). The `Mesh` field
///    is a script property carried in this tag loop (not a separate
///    `Ar << this->Mesh` call as Binja's pseudo-c initially suggested).
/// 2. `Ar.Ver() < 0x10`: a transient `TArray<FRawColorStream>` is read
///    into a stack local and immediately destroyed. Skipped at SC
///    (Ver=0x64 >= 0x10). Confirmed against LLIL `jge 0x104d6c92`
///    at xbe `0x104d6bd8`: the jump (skipping the body) fires for
///    `Ver >= 0x10`.
/// 3. `Ar.Ver() >= 0x11`: `Ar << this->ColorStream` where
///    `ColorStream` is an `FRawColorStream` value at `this+0x38`,
///    NOT a `UObject*` ref. The reader is `sub_10302b12` which
///    tail-calls `sub_10479820`, the global
///    `operator<<(FArchive&, FRawColorStream&)`.
///
/// `FRawColorStream::operator<<` (`sub_10479820`) at SC's
/// `GIsSavegame=0`:
///
/// * `if (*(0x10688c38) != 0)`: 8 raw bytes at `+0x10`. The flag is
///   the editor-only `GIsEditor`-style global, zero at runtime, so
///   this block is skipped at normal package load.
/// * `TArray<FColor>` at `+0x4` (`sub_103036de` = `sub_1047bdb0`):
///   packed_int count + per element 4 raw 1-byte reads (B, G, R, A).
/// * 4 raw bytes at `+0x18` (the stride / flag tail).
#[derive(Default, Debug)]
pub struct StaticMeshInstance {
    pub parent_object: Object,
    pub color_stream: FRawColorStream,
}

#[derive(Default, Debug, Clone)]
pub struct FRawColorStream {
    /// Per-vertex `FColor` values, 4 bytes each (B, G, R, A on disk).
    pub colors: Vec<u8>,
    /// 4-byte trailing field at `FRawColorStream+0x18` (engine treats
    /// this as a stride/flag word).
    pub trailing: u32,
}

impl DeserializeUnrealObject for StaticMeshInstance {
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
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let file_version = linker.borrow().version();

        if file_version >= 0x11 {
            // Inline FRawColorStream::operator<< body.
            let count = reader.read_packed_int()?;
            if count < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("UStaticMeshInstance FRawColorStream colors count negative ({count})"),
                ));
            }
            let total = (count as usize).saturating_mul(4);
            let mut colors = vec![0u8; total];
            if !colors.is_empty() {
                reader.cheat(&mut colors)?;
            }
            self.color_stream.colors = colors;
            self.color_stream.trailing = reader.read_u32::<E>()?;
        }

        Ok(())
    }
}
