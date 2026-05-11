use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;
use tracing::Level;
use tracing::debug;
use tracing::span;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirror of SC's `UMeshAnimation::Serialize`
/// (Engine_demo `0x10302d88` -> `sub_104a40d0`).
///
/// Verified against the binary's helper TArray readers
/// (`sub_104c2d70` for RefBones, `sub_104c2f70` for Moves,
/// `sub_104c3cb0` for AnimSeqs) plus the AnalogTrack chain
/// (`sub_104c3280` -> `sub_104c3770`/`sub_104c3950`/`sub_104c3b40`)
/// and the FMeshAnimNotify reader (`sub_104c3fa0`).
///
/// At SC's `Ver=0x64`, `LicenseeVer=0x11`, `IsLoading=1`, `IsPersistent=1`:
///
/// 1. `UObject::Serialize` (super, via `parent_object`): tagged-property
///    loop, terminates at the linker's "None" FName.
/// 2. `InternalVersion`: 4 raw bytes (DWORD).
/// 3. `RefBones`: `TArray<FNamedBone>`. Per element on disk:
///    FName(packed_int), DWORD Flags (4 raw), INT ParentIndex (4 raw).
/// 4. `Moves`: `TArray<MotionChunk>`. Per element on disk:
///    FVector RootSpeed3D (12 raw), FLOAT TrackTime (4 raw),
///    INT StartBone (4 raw), DWORD Flags (4 raw),
///    TArray<INT> BoneIndices (count + count*4),
///    TArray<AnalogTrack> AnimTracks,
///    AnalogTrack RootTrack (inline, no count prefix).
/// 5. `AnimSeqs`: `TArray<FMeshAnimSeq>`. Per element on disk
///    (in this exact order, independent of memory offset order):
///    FName Name (packed_int),
///    TArray<FName> Groups (count + count packed_ints),
///    INT StartFrame (4 raw),
///    INT NumFrames (4 raw),
///    TArray<FMeshAnimNotify> Notifys,
///    FLOAT Rate (4 raw),
///    BYTE trailing (1 raw): SC-specific extra byte.
///
/// `AnalogTrack` for SC's `Ver >= 0xd` path (`sub_104c4700`'s
/// `>= 0xd` branch, inlined inside `sub_104c3280` for AnimTracks
/// elements):
///        TArray<{4 u16}> KeyQuat-compressed (count + count*8)
///        TArray<FVector> KeyPos          (count + count*12)
///        TArray<u16>     KeyTime-compressed (count + count*2)
///    No Flags is read on disk in the `Ver >= 0xd` path; the in-memory
///    `Flags` field is zero-initialized.
///
/// `FMeshAnimNotify` per element on disk:
///        FLOAT Time (4 raw)
///        FName Function (packed_int)
///        FName Object   (packed_int): SC swapped UT's `UObject*
///                                      NotifyObject` for an FName slot.
#[derive(Default, Debug)]
pub struct MeshAnimation {
    pub parent_object: Object,
    pub internal_version: u32,
    pub ref_bones: Vec<FNamedBone>,
    pub moves: Vec<MotionChunk>,
    pub anim_seqs: Vec<FMeshAnimSeq>,
}

#[derive(Default, Debug, Clone)]
pub struct FNamedBone {
    pub name: i32,
    pub flags: u32,
    pub parent_index: i32,
}

#[derive(Default, Debug, Clone)]
pub struct MotionChunk {
    pub root_speed_3d: [f32; 3],
    pub track_time: f32,
    pub start_bone: i32,
    pub flags: u32,
    pub bone_indices: Vec<i32>,
    pub anim_tracks: Vec<AnalogTrack>,
    pub root_track: AnalogTrack,
}

#[derive(Default, Debug, Clone)]
pub struct AnalogTrack {
    /// Compressed quaternion keys: 8 bytes per element, 4 little-endian
    /// `u16` lanes. The compression scheme is the engine's own; we
    /// preserve the raw bytes so re-emit is byte-identical.
    pub key_quat: Vec<u8>,
    /// Position keys: 12 bytes per element (`FVector` = 3 little-endian
    /// floats).
    pub key_pos: Vec<u8>,
    /// Compressed time keys: 2 bytes per element (`u16` per key).
    pub key_time: Vec<u8>,
}

#[derive(Default, Debug, Clone)]
pub struct FMeshAnimSeq {
    pub name: i32,
    pub groups: Vec<i32>,
    pub start_frame: i32,
    pub num_frames: i32,
    pub notifys: Vec<FMeshAnimNotify>,
    pub rate: f32,
    pub trailing_byte: u8,
}

#[derive(Default, Debug, Clone)]
pub struct FMeshAnimNotify {
    pub time: f32,
    pub function: i32,
    pub object_name: i32,
}

/// Read `count * stride` raw bytes after a `packed_int` count prefix,
/// asserting the count is non-negative. Used for the fixed-stride TArrays
/// inside `MotionChunk` and `AnalogTrack`.
fn read_fixed_tarray<R: LinRead>(
    reader: &mut R,
    stride: usize,
    ctx: &'static str,
) -> io::Result<Vec<u8>> {
    let count = reader.read_packed_int()?;
    if count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UMeshAnimation {ctx} count negative ({count})"),
        ));
    }
    let total = (count as usize).saturating_mul(stride);
    if total == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; total];
    reader.cheat(&mut buf)?;
    Ok(buf)
}

fn read_analog_track<R>(reader: &mut R) -> io::Result<AnalogTrack>
where
    R: LinRead,
{
    // SC's `sub_104c3280` Ver>=0xd inline path skips the `Flags` slot on
    // disk; the in-memory field is zero-initialized rather than read.
    Ok(AnalogTrack {
        key_quat: read_fixed_tarray(reader, 8, "AnalogTrack KeyQuat")?,
        key_pos: read_fixed_tarray(reader, 12, "AnalogTrack KeyPos")?,
        key_time: read_fixed_tarray(reader, 2, "AnalogTrack KeyTime")?,
    })
}

fn read_motion_chunk<E, R>(reader: &mut R) -> io::Result<MotionChunk>
where
    E: ByteOrder,
    R: LinRead,
{
    let mut chunk = MotionChunk::default();
    for i in 0..3 {
        chunk.root_speed_3d[i] = reader.read_f32::<E>()?;
    }
    chunk.track_time = reader.read_f32::<E>()?;
    chunk.start_bone = reader.read_i32::<E>()?;
    chunk.flags = reader.read_u32::<E>()?;

    let bone_count = reader.read_packed_int()?;
    if bone_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UMeshAnimation MotionChunk BoneIndices count negative ({bone_count})"),
        ));
    }
    chunk.bone_indices.reserve(bone_count as usize);
    for _ in 0..bone_count {
        chunk.bone_indices.push(reader.read_i32::<E>()?);
    }

    let track_count = reader.read_packed_int()?;
    if track_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UMeshAnimation MotionChunk AnimTracks count negative ({track_count})"),
        ));
    }
    chunk.anim_tracks.reserve(track_count as usize);
    for _ in 0..track_count {
        chunk.anim_tracks.push(read_analog_track::<_>(reader)?);
    }

    chunk.root_track = read_analog_track::<_>(reader)?;
    Ok(chunk)
}

fn read_anim_notify<E, R>(reader: &mut R) -> io::Result<FMeshAnimNotify>
where
    E: ByteOrder,
    R: LinRead,
{
    Ok(FMeshAnimNotify {
        time: reader.read_f32::<E>()?,
        function: reader.read_packed_int()?,
        object_name: reader.read_packed_int()?,
    })
}

fn read_anim_seq<E, R>(reader: &mut R) -> io::Result<FMeshAnimSeq>
where
    E: ByteOrder,
    R: LinRead,
{
    let mut seq = FMeshAnimSeq {
        name: reader.read_packed_int()?,
        ..FMeshAnimSeq::default()
    };

    let group_count = reader.read_packed_int()?;
    if group_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UMeshAnimation FMeshAnimSeq Groups count negative ({group_count})"),
        ));
    }
    seq.groups.reserve(group_count as usize);
    for _ in 0..group_count {
        seq.groups.push(reader.read_packed_int()?);
    }

    seq.start_frame = reader.read_i32::<E>()?;
    seq.num_frames = reader.read_i32::<E>()?;

    let notify_count = reader.read_packed_int()?;
    if notify_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("UMeshAnimation FMeshAnimSeq Notifys count negative ({notify_count})"),
        ));
    }
    seq.notifys.reserve(notify_count as usize);
    for _ in 0..notify_count {
        seq.notifys.push(read_anim_notify::<E, _>(reader)?);
    }

    seq.rate = reader.read_f32::<E>()?;
    seq.trailing_byte = reader.read_u8()?;
    Ok(seq)
}

impl DeserializeUnrealObject for MeshAnimation {
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
        let span = span!(Level::DEBUG, "deserialize_mesh_animation");
        let _enter = span.enter();

        // 1. Super::Serialize (UObject tag-loop).
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        // 2. InternalVersion: 4 raw bytes.
        self.internal_version = reader.read_u32::<E>()?;

        // 3. RefBones.
        let bone_count = reader.read_packed_int()?;
        if bone_count < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UMeshAnimation RefBones count negative ({bone_count})"),
            ));
        }
        self.ref_bones.clear();
        self.ref_bones.reserve(bone_count as usize);
        for _ in 0..bone_count {
            self.ref_bones.push(FNamedBone {
                name: reader.read_packed_int()?,
                flags: reader.read_u32::<E>()?,
                parent_index: reader.read_i32::<E>()?,
            });
        }

        // 4. Moves.
        let move_count = reader.read_packed_int()?;
        if move_count < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UMeshAnimation Moves count negative ({move_count})"),
            ));
        }
        self.moves.clear();
        self.moves.reserve(move_count as usize);
        for _ in 0..move_count {
            self.moves.push(read_motion_chunk::<E, _>(reader)?);
        }

        // 5. AnimSeqs.
        let seq_count = reader.read_packed_int()?;
        if seq_count < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UMeshAnimation AnimSeqs count negative ({seq_count})"),
            ));
        }
        self.anim_seqs.clear();
        self.anim_seqs.reserve(seq_count as usize);
        for _ in 0..seq_count {
            self.anim_seqs.push(read_anim_seq::<E, _>(reader)?);
        }

        debug!(
            "MeshAnimation: internal_version={:#x}, ref_bones={}, moves={}, anim_seqs={}",
            self.internal_version,
            self.ref_bones.len(),
            self.moves.len(),
            self.anim_seqs.len(),
        );

        Ok(())
    }
}
