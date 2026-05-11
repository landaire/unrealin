use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::ulod_mesh::LodMesh;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirrors `USkeletalMesh::Serialize` (Engine_demo `0x104a46b0`)
/// for SC's archive constants `Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `GIsSavegame=0`, `IsPersistent=1`.
///
/// Verified directly against the LLIL of `0x104a46b0` and the inner
/// TArray serializers (`sub_1030eae0`, `sub_104c1cf0`, `sub_104c2040`,
/// `sub_104c2240`, `sub_104bff20`, `sub_104c19b0`, `sub_104c2400`,
/// `sub_104c2610`, `sub_104c2880`, `sub_104c04f0`, `sub_10417f60`,
/// `sub_104bfd20`, `sub_104bd2e0`, `sub_1033a110`). The Binary Ninja
/// pseudo-c flipped two gate conditionals (`Ver < 6` vs `Ver >= 6`,
/// and the `+0x25c` field's `operator<<` resolution); both are
/// corrected here from the actual LLIL flow.
///
/// 1. `ULodMesh::Serialize` (super, via `parent_object`).
/// 2. In-memory flag set (`*(this+0x5c) = 1`); no IO.
/// 3. `TArray<3 u32>` at `+0x10c` (`sub_1030eae0`): packed_int +
///    count*12 raw.
/// 4. `TArray<{FName + 56 raw}>` at `+0x124` (`sub_104c1cf0`):
///    packed_int outer count, per element `FName + 4 + 16 (FQuat
///    via `sub_104bd2e0`) + 12 (FVector via `sub_1033a110`) + 6*4`.
/// 5. `Ar << UObject*` at `+0x170` (`vtable[6]`, the `DefaultAnim`
///    ref). **CASCADE**: fires `MeshAnimation` load.
/// 6. 4 raw bytes at `+0x13c`.
/// 7. `TArray<{TArray<u16> + 4 raw}>` at `+0x158` (`sub_104c2040`).
/// 8. `TArray<2 u16 = 4 raw>` at `+0x164` (`sub_104c2240`).
/// 9. `TArray<FName>` at `+0x284` (`sub_104bff20`): packed_int +
///    count packed_ints.
/// 10. `TArray<FName>` at `+0x290` (`sub_104bff20`).
/// 11. `TArray<12 u32 = 48 raw>` at `+0x29c` (`sub_104c19b0`).
/// 12. (`Ver > 2`, fires for SC): `TArray<4 u32>` at `+0x224`.
/// 13. (`Ver > 2`): `TArray<5 u32>` at `+0x230` (the `Ver >= 9`
///     branch fires inside per element).
/// 14. (`Ver > 2`): `TArray<4 u32>` at `+0x23c` (same `Ver >= 9`
///     branch).
/// 15. (`Ver >= 6`, fires for SC; Binja flipped this gate):
///     - `TArray<88-byte = 9 u16>` at `+0x140` (`sub_104c04f0`):
///       per element 9*2=18 raw bytes (the `!IsPersistent`-gated
///       inner FRawIndexBuffer block is skipped at SC's
///       `IsPersistent=1`).
///     - `TArray<88-byte>` at `+0x14c` (same serializer).
///     - `TArray<u16>` at `+0x130` (`sub_10417f60`).
/// 16. (`Ver >= 7`, fires for SC):
///     - 4 raw bytes at `+0x248` (count_inner: bone-influence
///       row width).
///     - 4 raw bytes at `+0x24c` (count_outer: number of
///       influence rows).
///     - `TArray<FString>` at `+0x250` (`sub_104bfd20`).
///     - **`FString` at `+0x25c`** (`operator<<(FArchive&,
///       FString&)`, not `Ar << UObject*` as Binja's pseudo-c
///       suggested; the actual call resolves through
///       `[0x10688db4]` which is the FString thunk).
///     - 2D loop: `count_outer * count_inner` elements, each 3*4
///       raw + 16 (FQuat) = 28 bytes.
/// 17. (`!IsPersistent`, never fires for SC): the tail block
///     reading `+0x278` etc. is skipped.
#[derive(Default, Debug)]
pub struct SkeletalMesh {
    pub parent_object: LodMesh,
    pub default_anim: Option<RcUnrealObject>,
}

/// Reads a fixed-stride `TArray` body: `packed_int(count) +
/// count*stride` raw bytes.
fn read_fixed_tarray<R>(reader: &mut R, stride: usize, ctx: &'static str) -> io::Result<()>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    if count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("USkeletalMesh {ctx} count negative ({count}); body misaligned"),
        ));
    }
    let total = (count as usize).saturating_mul(stride);
    if total > 0 {
        let mut buf = vec![0u8; total];
        reader.cheat(&mut buf)?;
    }
    Ok(())
}

/// Reads `TArray<FName>` body. Each FName is a packed_int linker-
/// local index; FName resolution doesn't cross-package cascade.
fn read_tarray_fname<R>(reader: &mut R, ctx: &'static str) -> io::Result<()>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    if count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("USkeletalMesh {ctx} count negative ({count}); body misaligned"),
        ));
    }
    for _ in 0..count {
        let _ = reader.read_packed_int()?;
    }
    Ok(())
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
        // Step 1: super (ULodMesh chain).
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let file_version = linker.borrow().version();

        // Step 3.
        read_fixed_tarray(reader, 12, "step3 (+0x10c)")?;

        // Step 4: per element FName (packed_int) + 56 raw bytes.
        let count_4 = reader.read_packed_int()?;
        if count_4 < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("USkeletalMesh step4 (+0x124) count negative ({count_4})"),
            ));
        }
        for _ in 0..count_4 {
            let _name = reader.read_packed_int()?;
            let mut tail = [0u8; 56];
            reader.cheat(&mut tail)?;
        }

        // Step 5: DefaultAnim cascade.
        self.default_anim = reader.read_object::<E>(runtime, linker)?;

        // Step 6.
        let mut field_13c = [0u8; 4];
        reader.cheat(&mut field_13c)?;

        // Step 7: per element inner `TArray<u16>` + 4 raw bytes.
        let count_7 = reader.read_packed_int()?;
        if count_7 < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("USkeletalMesh step7 (+0x158) count negative ({count_7})"),
            ));
        }
        for _ in 0..count_7 {
            let inner_count = reader.read_packed_int()?;
            if inner_count < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("USkeletalMesh step7 inner count negative ({inner_count})"),
                ));
            }
            let inner_bytes = (inner_count as usize).saturating_mul(2);
            if inner_bytes > 0 {
                let mut inner = vec![0u8; inner_bytes];
                reader.cheat(&mut inner)?;
            }
            let mut tail = [0u8; 4];
            reader.cheat(&mut tail)?;
        }

        // Step 8.
        read_fixed_tarray(reader, 4, "step8 (+0x164)")?;

        // Steps 9, 10.
        read_tarray_fname(reader, "step9 (+0x284)")?;
        read_tarray_fname(reader, "step10 (+0x290)")?;

        // Step 11.
        read_fixed_tarray(reader, 48, "step11 (+0x29c)")?;

        // Steps 12-14, gated `Ver > 2`.
        if file_version > 2 {
            read_fixed_tarray(reader, 16, "step12 (+0x224)")?;
            read_fixed_tarray(reader, 20, "step13 (+0x230)")?;
            read_fixed_tarray(reader, 16, "step14 (+0x23c)")?;
        }

        // Step 15, gated `Ver >= 6` (Binja's pseudo-c flipped this
        // to `< 6`; the LLIL `if (eax s>= 6)` is the truth). Three
        // more TArray reads at SC.
        if file_version >= 6 {
            // TArray<88-byte FAnimMeshVertexStream-like> at +0x140.
            // Per element on disk at SC's IsPersistent=1: 9*2 = 18
            // raw bytes (the `!IsPersistent`-gated FRawIndexBuffer
            // tail is skipped).
            read_fixed_tarray(reader, 18, "step15a (+0x140)")?;
            // Same shape at +0x14c.
            read_fixed_tarray(reader, 18, "step15b (+0x14c)")?;
            // TArray<u16> at +0x130.
            read_fixed_tarray(reader, 2, "step15c (+0x130)")?;
        }

        // Step 16, gated `Ver >= 7`.
        if file_version >= 7 {
            // count_inner / count_outer (raw u32 little-endian).
            let count_inner = reader.read_u32::<E>()?;
            let count_outer = reader.read_u32::<E>()?;

            // TArray<FString> at +0x250.
            let count_fstr = reader.read_packed_int()?;
            if count_fstr < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("USkeletalMesh step16 TArray<FString> count negative ({count_fstr})"),
                ));
            }
            for _ in 0..count_fstr {
                let _ = reader.read_string()?;
            }

            // FString at +0x25c (NOT UObject*).
            let _ = reader.read_string()?;

            // 2D loop: count_outer x count_inner x 28 bytes raw.
            let total_2d = (count_outer as u64)
                .checked_mul(count_inner as u64)
                .and_then(|v| v.checked_mul(28))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "USkeletalMesh step16 2D loop overflow: outer={count_outer} inner={count_inner}"
                        ),
                    )
                })?;
            if total_2d > 0 {
                if total_2d > usize::MAX as u64 {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("USkeletalMesh step16 2D loop too large ({total_2d} bytes)"),
                    ));
                }
                let mut buf = vec![0u8; total_2d as usize];
                reader.cheat(&mut buf)?;
            }
        }

        // Step 17 (`!IsPersistent`) skipped.
        Ok(())
    }
}
