use std::io;

use byteorder::{ByteOrder, ReadBytesExt};

use crate::{
    de::RcLinker,
    object::{
        DeserializeUnrealObject, RcUnrealObject,
        internal::ge_partitioner::{GEPartitioner, read_ge_partitioner},
        ulevel_base::LevelBase,
    },
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirrors `ULevel::Serialize` (Engine_demo `sub_10410be0`).
///
/// SC archive constants: `Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `IsSaving=0`, `IsPersistent=1`, `GIsSavegame=0`.
///
/// Per-step (gates that always fire at SC are inlined; gates that
/// always skip are dropped):
/// 1. `ULevelBase::Serialize` (super).
/// 2. `if (GIsSavegame) { ... }` — SC at package load has
///    `GIsSavegame=0`; entire savegame block (GBackupModel, two
///    globals, `GCircCache` 65536 bytes, 8 raw bytes from
///    `+0x3964`) is **skipped**.
/// 3. `Ar << Model` — packed_int (UObject*).
/// 4. `if (Ar.Ver < 0x62) Ar << OldSpecs` — Ver=0x64, **skipped**.
/// 5. `Ar.Serialize(&ApproxTime, 4)` — raw u32.
/// 6. `Ar << FirstDeleted` — packed_int.
/// 7. Loop i=0..15: `Ar << TextBlocks[i]` — packed_int per
///    `UTextBuffer*` (NUM_LEVEL_TEXT_BLOCKS = 16).
/// 8. `if (Ar.Ver > 0x3E) Ar << *(this + 0xd0)` — Ver=0x64, fires.
///    The field at `+0xd0` is a **single FString** in SC, not the
///    UT2004 `TMap<FString, FString> TravelInfo`. Verified via LLIL
///    `0x10410d2b` — the indirect call resolves through
///    `[0x10688db4]` = `operator<<(FArchive&, FString&)`. UT2004's
///    TMap<FString,FString> would have been a TMap operator<<; SC's
///    layout collapsed it (or the field changed semantics entirely).
/// 9. `if (Model && !IsTrans) Ar.Preload(Model)` — `Preload` is
///    a no-op stub in SC (vtable[4] tail-calls a function that
///    immediately returns); no IO.
/// 10. `if (Ar.LicenseeVer > 0) Ar << *(this + 0x3914)` — SC
///     LicenseeVer=0x11 fires this. Verified via xbe RE
///     (`sub_8d310` calls `sub_150ff0` with `arg1+0x3914`): the
///     field is a `GEPartitioner`, SC's spatial structure for
///     "Geometric Events" (gore decals, dynamic destructibles,
///     etc.). Serialized inline via `internal::ge_partitioner`.
#[derive(Default, Debug)]
pub struct Level {
    pub parent_object: LevelBase,
    pub model: Option<RcUnrealObject>,
    pub approx_time: u32,
    pub first_deleted: Option<RcUnrealObject>,
    pub text_blocks: [Option<RcUnrealObject>; 16],
    pub travel_info: String,
    pub ge_partitioner: Option<GEPartitioner>,
}

impl DeserializeUnrealObject for Level {
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

        let licensee_version = linker.borrow().licensee_version();

        // Step 3: Model.
        self.model = reader.read_object::<E>(runtime, linker)?;

        // Step 5: ApproxTime (raw u32).
        self.approx_time = reader.read_u32::<E>()?;

        // Step 6: FirstDeleted.
        self.first_deleted = reader.read_object::<E>(runtime, linker)?;

        // Step 7: TextBlocks[16].
        for slot in self.text_blocks.iter_mut() {
            *slot = reader.read_object::<E>(runtime, linker)?;
        }

        // Step 8: single FString at +0xd0 (SC-specific; UT2004's
        // TMap<FString,FString> TravelInfo isn't what's here).
        self.travel_info = reader.read_string()?;

        // Step 9: `if (Model && !Ar.IsTrans()) Ar.Preload(Model)`.
        // ULinkerLoad::Preload synchronously seeks to Model's
        // serial_offset, deserializes Model's body, and seeks back.
        if let Some(model) = self.model.clone() {
            runtime.full_load_object::<E, _>(&model, reader)?;
        }

        // Step 10: SC LicenseeVer extension at `ULevel + 0x3914` is a
        // GEPartitioner, deserialized inline via xbe `sub_150ff0`.
        if licensee_version > 0 {
            self.ge_partitioner = Some(read_ge_partitioner::<E, _>(reader)?);
        }

        Ok(())
    }
}
