use std::io;

use byteorder::ByteOrder;

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, RcUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirrors `UStaticMeshInstance::Serialize` (Engine_demo `0x10305dcb`
/// tail-calls `sub_104d6ba0`). UStaticMeshInstance derives from
/// UMeshInstance, which has no Serialize override, so the parent path
/// is `UObject::Serialize` directly.
///
/// SC's archive constants (`Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `GIsSavegame=0`):
///
/// 1. `UObject::Serialize` (tagged-property loop).
/// 2. `Ar.Ver() < 0x10`: a transient `TArray<FRawColorStream>` is read
///    into a stack local and immediately destroyed. **Skipped at SC**
///    (Ver=0x64 > 0x10). Binja's pseudo-c renders the gate as
///    `if (eax s< 0x10)` and then the TArray body. That "if-then"
///    is in Binja's source-order, so the body runs only when the
///    condition is true. Confirmed against the LLIL `jge 0x104d6c92`
///    at xbe `0x104d6bd8`: the jump (skipping the body) fires for
///    `Ver >= 0x10`. Earlier versions of this stub had the gate
///    inverted and over-read 4-plus bytes per FRawColorStream
///    element on every StaticMeshInstance, which surfaced as a
///    `Read{4} vs trace Read{1}` panic on the first
///    `StaticMeshInstance0` in checked DM1.
/// 3. `Ar.Ver() >= 0x11`: `Ar << this->Mesh` (`UObject*` at `+0x38`).
///    Cascade trigger: the whole reason this stub exists vs.
///    falling through to plain Object's tag loop with a residual
///    cheat.
#[derive(Default, Debug)]
pub struct StaticMeshInstance {
    pub parent_object: Object,
    pub mesh: Option<RcUnrealObject>,
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
            self.mesh = reader.read_object::<E>(runtime, linker)?;
        }

        Ok(())
    }
}
