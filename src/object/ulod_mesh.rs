use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::umesh::Mesh;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirrors `ULodMesh::Serialize` (Engine_demo `0x10417bf0`).
/// Sequence per BinAssist decompile (the SC xbe analog has stripped
/// symbols; we lean on Engine_demo as the closest shipping binary
/// and on UT2004's `UnLodMesh.cpp::ULodMesh::Serialize` for cross-
/// reference).
///
/// All reads are unconditional at SC's `Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `GIsSavegame=0`. Inner TArray serializers are
/// raw-bytes (no FName / FString / FObject) except for the materials
/// array at `+0x70` which is `TArray<UObject*>` -- each element is a
/// packed_int routed through `read_object` so the cascade fires.
#[derive(Default, Debug)]
pub struct LodMesh {
    pub parent_object: Mesh,

    pub field_5c: u32,
    pub field_60: u32,
    pub field_64_array: Vec<u8>, // TArray<u32>, 4 B/elt (sub_10418260)
    pub faces_38b: Vec<u8>,      // TArray<38B-on-disk> (sub_104183f0)
    pub materials: Vec<Option<RcUnrealObject>>, // TArray<UObject*> (sub_104180e0)
    pub field_88: u32,
    pub field_8c: u32,
    pub field_90: u32,
    pub field_94: u32,
    pub field_98: u32,
    pub field_9c: u32,
    pub field_a0: u32,
    pub field_a4: u32,
    pub field_a8: u32,
    pub field_ac_array: Vec<u8>, // TArray<u16>
    pub field_b8_array: Vec<u8>, // TArray<u16>
    pub field_c4_array: Vec<u8>, // TArray<{4 u16}=8B>
    pub field_d0_array: Vec<u8>, // TArray<u16>
    pub field_dc_array: Vec<u8>, // TArray<10B-on-disk>
    pub field_e8_array: Vec<u8>, // TArray<{2 u32}=8B>
    pub field_f4: u32,
    pub field_108: u32,
    pub field_f8: u32,
    pub field_fc: u32,
    pub field_100: u32,
    pub field_104: u32,
}

fn read_packed_u8s<R>(reader: &mut R, stride: usize) -> io::Result<Vec<u8>>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    if count < 0 {
        // Negative TArray counts can't be written by the engine; reaching
        // this means our cursor is misaligned with the engine's read
        // sequence. Returning an error (rather than panicking) lets the
        // surrounding `preload`/`decode_unchecked` path unwind cleanly so
        // captured bodies upstream of this mismatch still make it into
        // `merge::write_packages`.
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("ULodMesh TArray count negative ({count}); body misaligned"),
        ));
    }
    let total = (count as usize) * stride;
    let mut buf = vec![0u8; total];
    if !buf.is_empty() {
        reader.cheat(&mut buf)?;
    }
    Ok(buf)
}

impl DeserializeUnrealObject for LodMesh {
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

        self.field_5c = reader.read_u32::<E>()?;
        self.field_60 = reader.read_u32::<E>()?;
        self.field_64_array = read_packed_u8s(reader, 4)?;
        self.faces_38b = read_packed_u8s(reader, 38)?;

        // TArray<UObject*> at +0x70: each element is a packed_int
        // routed through `read_object` so the cascade fires.
        let materials_count = reader.read_packed_int()?;
        assert!(materials_count >= 0);
        self.materials = (0..materials_count)
            .map(|_| reader.read_object::<E>(runtime, linker))
            .collect::<io::Result<Vec<_>>>()?;

        self.field_88 = reader.read_u32::<E>()?;
        self.field_8c = reader.read_u32::<E>()?;
        self.field_90 = reader.read_u32::<E>()?;
        self.field_94 = reader.read_u32::<E>()?;
        self.field_98 = reader.read_u32::<E>()?;
        self.field_9c = reader.read_u32::<E>()?;
        self.field_a0 = reader.read_u32::<E>()?;
        self.field_a4 = reader.read_u32::<E>()?;
        self.field_a8 = reader.read_u32::<E>()?;

        self.field_ac_array = read_packed_u8s(reader, 2)?;
        self.field_b8_array = read_packed_u8s(reader, 2)?;
        self.field_c4_array = read_packed_u8s(reader, 8)?;
        self.field_d0_array = read_packed_u8s(reader, 2)?;
        self.field_dc_array = read_packed_u8s(reader, 10)?;
        self.field_e8_array = read_packed_u8s(reader, 8)?;

        // Steps 22-27: six raw u32 in this on-disk order:
        // +0xf4, +0x108, +0xf8, +0xfc, +0x100, +0x104.
        self.field_f4 = reader.read_u32::<E>()?;
        self.field_108 = reader.read_u32::<E>()?;
        self.field_f8 = reader.read_u32::<E>()?;
        self.field_fc = reader.read_u32::<E>()?;
        self.field_100 = reader.read_u32::<E>()?;
        self.field_104 = reader.read_u32::<E>()?;

        Ok(())
    }
}
