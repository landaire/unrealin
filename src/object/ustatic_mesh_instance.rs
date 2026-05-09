use std::io;

use byteorder::{ByteOrder, ReadBytesExt};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, RcUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirrors `UStaticMeshInstance::Serialize` (Engine_demo `0x10305dcb`
/// tail-calls `sub_104d6ba0`). UStaticMeshInstance derives from
/// UMeshInstance which has no Serialize override, so the parent path is
/// `UObject::Serialize` directly.
///
/// SC's archive constants (`Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `GIsSavegame=0`) make both version gates fire:
///
/// 1. `UObject::Serialize` (tagged-property loop).
/// 2. `Ar.Ver() >= 0x10`: read a transient `TArray<FRawColorStream>`
///    into a stack local and immediately destroy it. The on-disk shape
///    is the only thing that matters here:
///    - outer `packed_int` count
///    - per element (`FRawColorStream::operator<<` at normal load —
///      `GIsSavegame=0` skips the savegame-only 8 bytes at +0x10):
///      - `TArray<FColor>` = `packed_int` + `count*4` bytes
///      - 4 raw bytes (the +0x18 trailing field)
///    No `Ar << UObject*` calls, so no cascade.
/// 3. `Ar.Ver() >= 0x11`: `Ar << this->Mesh` (`UObject*` at `+0x38`).
///    THIS is the cascade trigger; reading it via `read_object` is the
///    whole point of standing this stub up — `cheat()`-ing the body
///    opaquely would silently miss the Mesh ref and the cascade would
///    fail to fire when the ref points at a not-yet-loaded package.
#[derive(Default, Debug)]
pub struct StaticMeshInstance {
    pub parent_object: Object,
    pub color_streams: Vec<FRawColorStreamCapture>,
    pub mesh: Option<RcUnrealObject>,
}

#[derive(Default, Debug)]
pub struct FRawColorStreamCapture {
    pub colors: Vec<u8>,
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

        if file_version >= 0x10 {
            let outer = reader.read_packed_int()?;
            assert!(outer >= 0, "negative FRawColorStream outer count");
            self.color_streams = (0..outer)
                .map(|_| -> io::Result<FRawColorStreamCapture> {
                    let inner = reader.read_packed_int()?;
                    assert!(inner >= 0, "negative FColor inner count");
                    let mut colors = vec![0u8; (inner as usize) * 4];
                    if !colors.is_empty() {
                        reader.cheat(&mut colors)?;
                    }
                    let trailing = reader.read_u32::<E>()?;
                    Ok(FRawColorStreamCapture { colors, trailing })
                })
                .collect::<io::Result<Vec<_>>>()?;
        }

        if file_version >= 0x11 {
            self.mesh = reader.read_object::<E>(runtime, linker)?;
        }

        Ok(())
    }
}
