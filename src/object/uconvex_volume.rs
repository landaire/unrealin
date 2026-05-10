use std::io;

use byteorder::{ByteOrder, ReadBytesExt};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, uprimitive::Primitive},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirror of `UConvexVolume::Serialize` (Engine_demo `0x10304818`
/// tail-calling `sub_103e3a40`). The shape:
///
/// 1. Super: `UPrimitive::Serialize` (`0x10304ac0`), which itself
///    calls `UObject::Serialize` (tagged-property loop) then reads
///    FBox(25) + FSphere(16 at `Ar.Ver() > 0x3d`).
/// 2. `TArray<FConvexVolumeFace>` at `+0x58`
///    (`sub_10306348` -> `sub_103e3b70`, in-memory stride 0x1c). On
///    disk per element: 16-byte FPlane (4 raw u32s) + nested
///    `TArray<FVector>` (`sub_1030eae0` -> `sub_1030eae0`, stride
///    0xc) at `+0x10`.
/// 3. `TArray<FPlane>` at `+0x64`
///    (`sub_10305b6e` -> `sub_103e3e10`, stride 0x10). Per element
///    4 raw u32s.
/// 4. 6 raw u32s at `+0x70`, `+0x74`, `+0x78`, `+0x7c`, `+0x80`,
///    `+0x84`.
/// 5. 1 raw byte at `+0x88`.
///
/// All TArray element counts are encoded as `FCompactIndex` (the
/// `operator<<(FArchive&, FCompactIndex&)` global at `[0x10688b88]`)
/// per UE2's `TArray<T>::operator<<` template.
#[derive(Default, Debug)]
pub struct ConvexVolume {
    pub parent_object: Primitive,
}

fn read_fixed_tarray<R>(reader: &mut R, stride: usize, ctx: &'static str) -> io::Result<()>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    if count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UConvexVolume {ctx} count negative ({count}); body misaligned"),
        ));
    }
    let total = (count as usize).saturating_mul(stride);
    if total > 0 {
        let mut buf = vec![0u8; total];
        reader.cheat(&mut buf)?;
    }
    Ok(())
}

impl DeserializeUnrealObject for ConvexVolume {
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
        // 1. Super.
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        // 2. TArray<FConvexVolumeFace> at +0x58. Each face on disk is
        //    16 bytes (FPlane) + a nested TArray<FVector>.
        let face_count = reader.read_packed_int()?;
        if face_count < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "UConvexVolume face TArray (+0x58) count negative ({face_count})"
                ),
            ));
        }
        for _ in 0..face_count {
            let mut plane = [0u8; 16];
            reader.cheat(&mut plane)?;
            read_fixed_tarray(reader, 12, "face verts (FPlane.+0x10)")?;
        }

        // 3. TArray<FPlane> at +0x64.
        read_fixed_tarray(reader, 16, "planes (+0x64)")?;

        // 4. 6 raw u32s at +0x70..+0x88.
        let mut tail_floats = [0u8; 24];
        reader.cheat(&mut tail_floats)?;

        // 5. 1 raw byte at +0x88.
        let _flag = reader.read_u8()?;

        Ok(())
    }
}
