use std::io;

use byteorder::ByteOrder;

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, ulod_mesh::LodMesh},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirrors `USkeletalMesh::Serialize` (Engine_demo `0x104a46b0`)
/// for SC's archive constants `Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `GIsSavegame=0`, `IsPersistent=1`.
///
/// Only the prefix steps 3-5 are implemented here pending precise
/// SC-xbe verification of the variable-stride TArrays and the
/// nested 2D table at step 16. Steps 3-5 are the steps that
/// (a) read fixed-size data we are confident in, and (b) include
/// the `DefaultAnim` `Ar << UObject*` (step 5) — the load that
/// triggers the SkeletalMesh -> MeshAnimation cascade.
///
/// Step 17 SKIPS at normal load (gate is `Ar[7] == 0` =
/// `!IsPersistent`). UT2004 source `UnSkeletalMesh.cpp:465`
/// confirms with `if (!Ar.IsPersistent())`.
#[derive(Default, Debug)]
pub struct SkeletalMesh {
    pub parent_object: LodMesh,
}

impl DeserializeUnrealObject for SkeletalMesh {
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
        self.parent_object.deserialize::<E, _>(runtime, linker, reader)?;

        // Step 3: TArray<12B = 3 u32> at +0x10c (sub_1030eae0).
        let count_3 = reader.read_packed_int()?;
        assert!(count_3 >= 0);
        let mut buf_3 = vec![0u8; (count_3 as usize) * 12];
        if !buf_3.is_empty() {
            reader.cheat(&mut buf_3)?;
        }

        // Step 4: TArray<FName + 14 u32 = 56 bytes raw> at +0x124
        // (sub_104c1cf0). FName via vtbl[7] is per-byte packed_int;
        // the 56 bytes follow per-element.
        let count_4 = reader.read_packed_int()?;
        assert!(count_4 >= 0);
        for _ in 0..count_4 {
            let _name = reader.read_packed_int()?;
            let mut tail = [0u8; 56];
            reader.cheat(&mut tail)?;
        }

        // Step 5: Ar << UObject* at +0x170. This is the
        // `DefaultAnim` ref (USkelMesh -> MeshAnimation cascade).
        let _default_anim = reader.read_object::<E>(runtime, linker)?;

        // Steps 6-16 omitted pending precise verification.
        // `preload`'s short-read fallback consumes the residual.
        Ok(())
    }
}
