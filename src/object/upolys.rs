use std::io;

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, span};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, RcUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirror of SC's `UPolys::Serialize` (Engine_demo `0x10303e6d` tail-calls
/// `0x10310f50`). For the load path (`arg2[6] == 0`, i.e. not saving):
///
/// 1. `UObject::Serialize` (tagged-property loop on the parent UObject).
/// 2. `FArchive::CountBytes` (vtable+0x14) — memory accounting only, no IO.
/// 3. `Ar.Serialize(&Max, 4)` — 4 raw bytes (TArray Max field).
/// 4. `Ar.Serialize(&Count, 4)` — 4 raw bytes (TArray Count field).
/// 5. (Skipped on normal load) GUndo bookkeeping; reads more for savegame.
/// 6. For each poly (Count iterations): `operator<<(Ar, FPoly&)`
///    (`0x10420370`).
///
/// Note: even though the on-disk format is a `TArray<FPoly>`, the engine
/// uses 4-byte INTs for Max/Count here (not the usual packed_int prefix).
/// The QEMU trace for SC's xbox runtime confirms `Read(4)+Read(4)` here.
#[derive(Default, Debug)]
pub struct Polys {
    pub parent_object: Object,
    pub max: u32,
    pub polys: Vec<FPoly>,
}

/// Mirror of SC's `operator<<(FArchive&, FPoly&)` (`0x10420370`) on the
/// `Ver >= 0x4E, GIsSavegame == 0` path, which is what SC's xbox runtime
/// uses for normal package loads:
///
/// 1. `Ar << num_vertices` — packed_int.
/// 2. 12 individual `Ar.Serialize(buf, 4)` reads for the four FVectors
///    (Base, Normal, TextureU, TextureV) -> 48 raw bytes total.
/// 3. Per vertex (`num_vertices` iterations): three `Ar.Serialize(buf, 4)`
///    reads for X/Y/Z -> 12 raw bytes per vertex.
/// 4. `Ar.Serialize(buf, 4)` -> `PolyFlags`.
/// 5. `Ar << UObject*` -> `Actor`.
/// 6. `Ar << UObject*` -> `Material` (the texture/shader reference; this
///    is the call that enqueues per-poly materials like `Beam_TRA`).
/// 7. `Ar << FName` -> `ItemName` (encoded as packed_int on disk).
/// 8. `Ar << INT` -> `iLink` (packed_int).
/// 9. `Ar << INT` -> `iBrushPoly` (packed_int).
///
/// (The `Ver >= 0x4E && GIsSavegame != 0` branch reads two more 2-byte
/// fields and a re-orient computation; we don't model it.)
#[derive(Default, Debug)]
pub struct FPoly {
    /// Base, Normal, TextureU, TextureV — four FVectors, 48 bytes total.
    /// Stored as a Vec rather than `[u8; 48]` because Default for arrays
    /// > 32 needs an explicit impl, which isn't worth it here.
    pub vectors_raw: Vec<u8>,
    pub vertices_raw: Vec<u8>,
    pub poly_flags: u32,
    pub actor: Option<RcUnrealObject>,
    pub material: Option<RcUnrealObject>,
    pub item_name: i32,
    pub i_link: i32,
    pub i_brush_poly: i32,
}

impl DeserializeUnrealObject for Polys {
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
        let span = span!(Level::DEBUG, "deserialize_polys");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        self.max = reader.read_u32::<E>()?;
        let count = reader.read_u32::<E>()? as i32;
        assert!(count >= 0, "negative Polys count");

        self.polys = (0..count)
            .map(|_| -> io::Result<FPoly> {
                let num_vertices = reader.read_packed_int()?;
                assert!(num_vertices >= 0, "negative FPoly vertex count");

                let mut vectors_raw = vec![0u8; 48];
                reader.cheat(&mut vectors_raw)?;

                let mut vertices_raw = vec![0u8; (num_vertices as usize) * 12];
                if !vertices_raw.is_empty() {
                    reader.cheat(&mut vertices_raw)?;
                }

                let poly_flags = reader.read_u32::<E>()?;
                let actor = reader.read_object::<E>(runtime, linker)?;
                let material = reader.read_object::<E>(runtime, linker)?;
                let item_name = reader.read_packed_int()?;
                let i_link = reader.read_packed_int()?;
                let i_brush_poly = reader.read_packed_int()?;

                Ok(FPoly {
                    vectors_raw,
                    vertices_raw,
                    poly_flags,
                    actor,
                    material,
                    item_name,
                    i_link,
                    i_brush_poly,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;

        Ok(())
    }
}
