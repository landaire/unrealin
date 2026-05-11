use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::uprimitive::Primitive;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirror of `UESoftBody::Serialize` (Engine_demo `0x10306d98`
/// tail-calling `sub_103b59d0`). UESoftBody is the native SoftBody
/// base for SC's scripted physics-cloth classes (ESBPatch, ESBRope,
/// ESBChain, ESBStripDoor); none of those add their own native
/// Serialize, so they all flow through this body verbatim and the
/// virtual `SerializeExtra` (vtable[0x84], `sub_103b5f40`) is a bare
/// `ret`.
///
/// At SC's archive constants (`Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `IsTrans=0`, `IsPersistent=1`, `GIsSavegame=0`,
/// `GIsEditor=0`) the editor-only branch at `0x103b5a17` (gated on
/// `GIsEditor != 0 && Ar.IsTrans != 0`) does not fire; only the
/// runtime path at `0x103b5aa1` runs.
///
/// 1. Super: UPrimitive::Serialize (`0x10304ac0` -> `sub_10451fa0`),
///    which itself calls `UObject::Serialize` (`[0x10688b94]`) for
///    the tagged-property loop, then reads FBox(25) + FSphere(16 at
///    `Ar.Ver() > 0x3d`).
/// 2. 4 raw bytes at `+0x128`.
/// 3. 4 raw bytes at `+0x12c`.
/// 4. `Ar << UObject*&` at `+0x5c` (vtable+0x18). CASCADE: the
///    referenced UObject is loaded.
/// 5. 4 raw bytes at `+0x58`. This is `V`, the per-instance
///    SoftBody disk version; all subsequent gates compare against
///    it (the LLIL stores `&this->v58` into the `arg2` stack slot
///    at `0x103b5ae0` and dereferences that slot in each gate).
/// 6. TArray at `+0x94` (`sub_10305c40` -> `sub_103b6d00`): per
///    element 72 raw bytes (10 raw u32s + FVector + FVector + 2
///    raw u32s).
/// 7. TArray at `+0xa0` (`sub_10304ff7` -> `sub_103b70b0`): per
///    element 12 raw bytes (FVector).
/// 8. TArray at `+0xac` (`sub_103037fb` -> `sub_103b72b0`): per
///    element 16 raw bytes (4 raw u32s).
/// 9. TArray at `+0xd0` (`sub_103017d0` -> `sub_103b6ae0`): per
///    element `Ar << UObject*&` (packed_int linker index, vtable
///    +0x18 cascade) + 12 raw bytes.
/// 10. 15 raw u32s in this on-disk order: `+0xdc`, `+0xe0`,
///     `+0xe4`, `+0xe8`, `+0xec`, `+0xf0`, `+0xf4`, `+0xf8`,
///     `+0xfc`, `+0x100`, `+0x104`, `+0x108`, `+0x10c`, `+0x110`,
///     `+0x11c`.
/// 11. (V > 2): 4 each at `+0x13c`, `+0x140`.
/// 12. (V > 3): 4 at `+0x120`.
/// 13. (V > 4): TArray at `+0xb8` (`sub_10304c3c` ->
///     `sub_103b74c0`), per element 25 raw bytes (6 raw u32s + 1
///     raw byte; the in-memory stride is 28 with 3 bytes of
///     trailing padding) + TArray at `+0xc4` (`sub_10305060` ->
///     `sub_103b7760`), per element 16 raw bytes.
/// 14. (V > 5): 6 raw u32s at `+0x60`, `+0x64`, `+0x68`, `+0x6c`,
///     `+0x70`, `+0x74` + 1 raw byte at `+0x78`.
/// 15. (V > 6): 4 at `+0x124`.
/// 16. (V > 7): 4 each at `+0x114`, `+0x118`.
/// 17. (V > 8): 4 at `+0x130`.
/// 18. (V > 9): 6 raw u32s at `+0x7c`, `+0x80`, `+0x84`, `+0x88`,
///     `+0x8c`, `+0x90`.
/// 19. (V > 0xa): 4 each at `+0x134`, `+0x138`.
/// 20. SerializeExtra (vtable[0x84], no-op for UESoftBody and the
///     script subclasses).
///
/// The TArray helpers (`sub_103b6d00`, `sub_103b70b0`, etc.) all
/// share the same shape on the load path: packed_int via global
/// `[0x10688b88]` (`operator<<(FArchive&, FCompactIndex&)`), then
/// per element `count` 4-byte / 1-byte / cascade reads. We collapse
/// the fixed-stride ones to `read_packed_int` + `cheat(stride*count)`
/// for byte-identical behaviour.
#[derive(Default, Debug)]
pub struct SoftBody {
    pub parent_object: Primitive,
    pub disk_version: u32,
    /// Cascade ref read at `+0x5c`. Holding it keeps the load order
    /// reproducible and lets future serialize impls re-emit it.
    pub mesh_ref: Option<RcUnrealObject>,
    /// Per-element UObject refs from the `+0xd0` TArray. The 12
    /// trailing bytes per element are part of the captured-byte
    /// stream we're not modelling field-wise yet.
    pub attachments: Vec<Option<RcUnrealObject>>,
}

fn read_fixed_tarray<R>(reader: &mut R, stride: usize, ctx: &'static str) -> io::Result<()>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    if count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UESoftBody {ctx} count negative ({count}); body misaligned"),
        ));
    }
    let total = (count as usize).saturating_mul(stride);
    if total > 0 {
        let mut buf = vec![0u8; total];
        reader.cheat(&mut buf)?;
    }
    Ok(())
}

impl DeserializeUnrealObject for SoftBody {
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
        // 1. Super (UPrimitive::Serialize -> UObject tag-loop +
        //    FBox + FSphere).
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        // 2, 3. Two raw u32s at +0x128, +0x12c.
        let _ = reader.read_u32::<E>()?;
        let _ = reader.read_u32::<E>()?;

        // 4. Ar << UObject*& at +0x5c. CASCADE.
        self.mesh_ref = reader.read_object::<E>(runtime, linker)?;

        // 5. Per-instance disk version at +0x58.
        self.disk_version = reader.read_u32::<E>()?;
        let v = self.disk_version as i32;
        // Splinter Cell 1 Sep-2002 prototype's UESoftBody::Serialize
        // (`splintercell_proto.xbe UESoftBody__Serialize` at 0x61a90)
        // has the same field set and order as the demo's
        // `sub_103b59d0` but ZERO version gates: every gated field
        // below is read unconditionally. Proto stamps every save at
        // V=9 (its only on-disk value), so demo gates that fire at
        // V > 9 / V > 0xa miss the trailing fields for proto saves.
        //
        // Auto-upgrade detection: V==9 in a SoftBody body is a
        // reliable proto marker -- retail/demo bumped the version
        // past 9 the moment the gates landed. Once detected, persist
        // it on the runtime so any other proto-aware path (e.g. PT-
        // style branches that should also fire for proto) sees the
        // right game without re-deriving it.
        if v == 9 && runtime.game == crate::de::Game::SplinterCell {
            runtime.game = crate::de::Game::SplinterCellPrototype;
        }
        let proto_no_gates = runtime.game == crate::de::Game::SplinterCellPrototype;

        // 6. TArray<72-byte> at +0x94.
        read_fixed_tarray(reader, 72, "step6 (+0x94)")?;

        // 7. TArray<FVector> at +0xa0.
        read_fixed_tarray(reader, 12, "step7 (+0xa0)")?;

        // 8. TArray<16-byte> at +0xac.
        read_fixed_tarray(reader, 16, "step8 (+0xac)")?;

        // 9. TArray<{UObject*+12 raw}> at +0xd0. Per element:
        //    Ar << UObject*& (packed_int linker index, cascade) +
        //    12 raw bytes.
        let attach_count = reader.read_packed_int()?;
        if attach_count < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UESoftBody step9 (+0xd0) count negative ({attach_count})"),
            ));
        }
        self.attachments.clear();
        self.attachments.reserve(attach_count as usize);
        for _ in 0..attach_count {
            let obj = reader.read_object::<E>(runtime, linker)?;
            self.attachments.push(obj);
            let mut tail = [0u8; 12];
            reader.cheat(&mut tail)?;
        }

        // 10. 15 raw u32s in disk order: +0xdc..+0x110 contiguous
        //     plus a separate +0x11c at the end.
        let mut step10 = [0u8; 60];
        reader.cheat(&mut step10)?;

        // 11. (V > 2): 4 each at +0x13c, +0x140.
        if v > 2 {
            let mut buf = [0u8; 8];
            reader.cheat(&mut buf)?;
        }

        // 12. (V > 3): 4 at +0x120.
        if v > 3 {
            let _ = reader.read_u32::<E>()?;
        }

        // 13. (V > 4): TArray<28-byte struct, 25 disk bytes> at
        //     +0xb8 + TArray<16-byte> at +0xc4.
        if v > 4 {
            read_fixed_tarray(reader, 25, "step13a (+0xb8)")?;
            read_fixed_tarray(reader, 16, "step13b (+0xc4)")?;
        }

        // 14. (V > 5): 6 raw u32s + 1 raw byte = 25 bytes at
        //     +0x60..+0x78.
        if v > 5 {
            let mut buf = [0u8; 25];
            reader.cheat(&mut buf)?;
        }

        // 15. (V > 6): 4 at +0x124.
        if v > 6 {
            let _ = reader.read_u32::<E>()?;
        }

        // 16. (V > 7): 4 each at +0x114, +0x118.
        if v > 7 {
            let mut buf = [0u8; 8];
            reader.cheat(&mut buf)?;
        }

        // 17. (V > 8): 4 at +0x130.
        if v > 8 {
            let _ = reader.read_u32::<E>()?;
        }

        // 18. (V > 9): 6 raw u32s at +0x7c..+0x90.
        if v > 9 || proto_no_gates {
            let mut buf = [0u8; 24];
            reader.cheat(&mut buf)?;
        }

        // 19. (V > 0xa): 4 each at +0x134, +0x138.
        if v > 0xa || proto_no_gates {
            let mut buf = [0u8; 8];
            reader.cheat(&mut buf)?;
        }

        // 20. SerializeExtra (vtable[0x84]) is a no-op for
        //     UESoftBody and all four script subclasses.
        Ok(())
    }
}

/// Script subclasses of UESoftBody. None override SerializeExtra,
/// so their disk layout is identical to UESoftBody's.
macro_rules! soft_body_subclass {
    ($name:ident) => {
        #[derive(Default, Debug)]
        pub struct $name {
            pub parent_object: SoftBody,
        }

        impl DeserializeUnrealObject for $name {
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
                    .deserialize::<E, _>(runtime, linker, reader)
            }
        }
    };
}

soft_body_subclass!(ESBPatch);
soft_body_subclass!(ESBRope);
soft_body_subclass!(ESBChain);
soft_body_subclass!(ESBStripDoor);
