use std::io;
use std::io::SeekFrom;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;
use tracing::Level;
use tracing::debug;
use tracing::span;

use crate::de::ExportIndex;
use crate::de::Linker;
use crate::de::RcLinker;
use crate::object::BodyKind;
use crate::object::BodyOffsetPatch;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::SerializeUnrealObject;
use crate::object::uprimitive::Primitive;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirror of SC's `UStaticMesh::Serialize` (Engine_demo `0x10303dc3`
/// tail-calls `sub_104d4010`; proto XBE `UStaticMesh__Serialize`
/// at `0xeafb0`). UT2004's `UnStaticMesh.cpp` is a useful naming guide
/// for `Sections`, `RawTriangles`, and `InternalVersion`, but SC changed
/// the render/collision arrays around those fields.
///
/// The retail/demo body is heavily version-gated; SC's archive header is
/// `Ver=0x64`, `LicenseeVer=0x11`, so the active load-time path is:
///
/// 1. `UPrimitive::Serialize` (UObject tagged-property loop + 41 bytes of
///    BBox/Sphere).
/// 2. `Ar.Ver() < 0x5C`: two `Ar << UObject*` (refs at +0xb4, +0xb8).
///    Skipped for SC.
/// 3. `Ar.LicenseeVer() >= 0x10`:
///    - mesh-level `FStaticMeshVertexStream.Vertices` at +0xc8
///      (`TArray<32-byte>`: FVector position, FVector normal, one UV pair).
///    - `FStaticMeshVertexStream.Revision` at +0xdc.
///    - `FRawIndexBuffer` at +0xe0 (`sub_10479690`: TArray<INT16> +
///      4 raw bytes; the savegame-only 8 bytes at +0x10 are skipped at
///      normal package-load time).
///    - `FRawIndexBuffer` at +0xfc (same layout).
/// 4. Always: `TArray<FStaticMeshSection>` at +0x58 (`sub_104dbd30`):
///    per-section is the `Ver >= 0x5C, LicenseeVer >= 0x10` branch of
///    `operator<<(Ar, FStaticMeshSection)` (`sub_103940a0`): seven raw
///    fields totaling 18 bytes (4+2+2+2+2+2+4) followed by `Ar << UObject*`.
///    Then 12, 12, and 1 raw bytes (FBox-shaped fields at +0x2c, +0x38,
///    +0x44).
/// 5. `Ar.Ver() >= 0x4A && Ar.LicenseeVer() >= 0xE`:
///    - SC collision primitive entries at +0x70 (`sub_104dbf30` /
///      proto `UStaticMesh__SerializeCollisionArray64`): each entry has
///      `TArray<u16>` children and either four u16 leaf fields or a
///      25-byte bounds-like struct.
///    - pooled collision vertices at +0x7c (`TArray<FVector>`).
///    - collision material refs at +0x88 (`TArray<UObject*>`).
/// 6. `Ar.Ver() >= 0x61`: `RawTriangles` `TLazyArray` skip.
///    `UStaticMesh` flips `*GLazyLoad` to 1 around the call so `Load()` never runs;
///    effective IO is just the 4-byte `SeekPos` and the trailing
///    `Ar.Seek(SeekPos)`. The lazy body is fully elided from the `.lin`,
///    so the seek is purely virtual bookkeeping (no source bytes to
///    consume).
/// 7. `Ar.Ver() >= 0x51`: `InternalVersion` at +0xac. If that value is
///    `>= 8`, SC reads one more 4-byte field at +0xb0; otherwise the
///    engine walks the outer chain and tags a `UPackage` flag in memory.
///
/// Proto keeps the same broad UT-derived concepts but stores them at
/// different offsets and has no tagged-property loop in `UObject::Serialize`.
/// In the prototype we still see the inherited `Primitive` bytes in the
/// recorded package stream, then SC's render arrays, sections, bounding box,
/// collision arrays, object refs, `RawTriangles` lazy seek, and the trailing
/// `InternalVersion` pair.
#[derive(Default, Debug)]
pub struct StaticMesh {
    pub parent_object: Primitive,

    pub material_a: Option<RcUnrealObject>,
    pub material_b: Option<RcUnrealObject>,

    pub vertex_stream_vertices: Vec<u8>,
    pub vertex_stream_revision: u32,
    pub index_buffer_a: FRawIndexBuffer,
    pub index_buffer_b: FRawIndexBuffer,

    pub sections: Vec<FStaticMeshSection>,

    pub field_2c: [u8; 12],
    pub field_38: [u8; 12],
    pub field_44: u8,

    pub collision_primitives: Vec<FStaticMeshCollisionPrimitive>,
    pub collision_vertices: Vec<u8>,
    pub collision_materials: Vec<Option<RcUnrealObject>>,

    pub lazy_seek_pos: i32,
    /// Body-relative byte offset of the `lazy_seek_pos` field within
    /// the captured StaticMesh body. `serialize` uses this to emit a
    /// `BodyOffsetPatch` so ser.rs can rewrite the stale absolute
    /// offset when the body lands at a new file position on re-emit.
    /// 0 if the version-gate at `Ver >= 0x61` doesn't fire (no patch
    /// needed in that case).
    pub lazy_seek_field_pos: usize,
    pub lazy_seek_target_capture_pos: usize,
    pub internal_version: u32,
    pub internal_version_payload: u32,
}

#[derive(Default, Debug)]
pub struct FRawIndexBuffer {
    pub indices: Vec<u8>,
    pub trailing: u32,
}

#[derive(Default, Debug)]
pub struct FStaticMeshSection {
    pub object: Option<RcUnrealObject>,
    pub raw: [u8; 18],
}

#[derive(Default, Debug)]
pub struct FStaticMeshCollisionPrimitive {
    pub indices: Vec<u8>,
    pub tail: Vec<u8>,
}

fn read_packed_nonnegative<R>(reader: &mut R, what: &str) -> io::Result<usize>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    if count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{what} count negative ({count})"),
        ));
    }
    Ok(count as usize)
}

fn read_chunks<R>(reader: &mut R, buf: &mut [u8], chunk_len: usize) -> io::Result<()>
where
    R: LinRead,
{
    for chunk in buf.chunks_exact_mut(chunk_len) {
        reader.read_exact(chunk)?;
    }
    Ok(())
}

fn read_static_mesh_vertices<R>(reader: &mut R, vertex_count: usize) -> io::Result<Vec<u8>>
where
    R: LinRead,
{
    let mut vertices = vec![0u8; vertex_count * 0x20];
    read_chunks(reader, &mut vertices, 4)?;
    Ok(vertices)
}

fn read_section_raw<R>(reader: &mut R) -> io::Result<[u8; 18]>
where
    R: LinRead,
{
    let mut raw = [0u8; 18];
    let mut offset = 0;
    for len in [4, 2, 2, 2, 2, 2, 4] {
        reader.read_exact(&mut raw[offset..offset + len])?;
        offset += len;
    }
    Ok(raw)
}

fn read_raw_index_buffer<E, R>(reader: &mut R) -> io::Result<FRawIndexBuffer>
where
    E: ByteOrder,
    R: LinRead,
{
    let count = read_packed_nonnegative(reader, "FRawIndexBuffer")?;
    let mut indices = vec![0u8; count * 2];
    if !indices.is_empty() {
        reader.cheat(&mut indices)?;
    }
    let trailing = reader.read_u32::<E>()?;
    Ok(FRawIndexBuffer { indices, trailing })
}

fn read_raw_index_buffer_recorded<E, R>(reader: &mut R) -> io::Result<FRawIndexBuffer>
where
    E: ByteOrder,
    R: LinRead,
{
    let count = read_packed_nonnegative(reader, "proto FRawIndexBuffer")?;
    let mut indices = vec![0u8; count * 2];
    if !indices.is_empty() {
        read_chunks(reader, &mut indices, 2)?;
    }
    let trailing = reader.read_u32::<E>()?;
    Ok(FRawIndexBuffer { indices, trailing })
}

fn read_static_mesh_collision_primitive<R>(
    reader: &mut R,
) -> io::Result<FStaticMeshCollisionPrimitive>
where
    R: LinRead,
{
    let index_count = read_packed_nonnegative(reader, "proto StaticMesh collision index")?;
    let mut indices = vec![0u8; index_count * 2];
    if !indices.is_empty() {
        reader.cheat(&mut indices)?;
    }

    let tail_len = if index_count == 0 { 8 } else { 25 };
    let mut tail = vec![0u8; tail_len];
    reader.cheat(&mut tail)?;
    Ok(FStaticMeshCollisionPrimitive { indices, tail })
}

fn read_static_mesh_collision_primitive_recorded<R>(
    reader: &mut R,
) -> io::Result<FStaticMeshCollisionPrimitive>
where
    R: LinRead,
{
    let index_count = read_packed_nonnegative(reader, "proto StaticMesh collision index")?;
    let mut indices = vec![0u8; index_count * 2];
    if !indices.is_empty() {
        read_chunks(reader, &mut indices, 2)?;
    }

    let tail_len = if index_count == 0 { 8 } else { 25 };
    let mut tail = vec![0u8; tail_len];
    if index_count == 0 {
        read_chunks(reader, &mut tail, 2)?;
    } else {
        read_chunks(reader, &mut tail[..24], 4)?;
        reader.read_exact(&mut tail[24..])?;
    }
    Ok(FStaticMeshCollisionPrimitive { indices, tail })
}

impl SerializeUnrealObject for StaticMesh {
    /// Patch the stale absolute `lazy_seek_pos` to its new position.
    /// Same pattern as `Texture::serialize` -- only the 4-byte
    /// TLazyArray skip target gets rewritten; everything else in the
    /// body stays verbatim. If the version gate didn't fire (no
    /// lazy_seek_pos was read), the recorded field position is 0 and
    /// we fall back to Opaque.
    fn serialize<E>(
        &self,
        linker: &Linker,
        export_index: ExportIndex,
        captured: &[u8],
    ) -> std::io::Result<BodyKind>
    where
        E: byteorder::ByteOrder,
    {
        if self.lazy_seek_field_pos == 0 {
            return Ok(BodyKind::Opaque(captured.to_vec()));
        }
        let target = if self.lazy_seek_target_capture_pos != 0 {
            self.lazy_seek_target_capture_pos
        } else {
            let old_body_offset = linker
                .find_export_by_index(export_index)
                .map(|e| e.serial_offset())
                .unwrap_or(0);
            (self.lazy_seek_pos as u64).saturating_sub(old_body_offset) as usize
        };
        Ok(BodyKind::Patched {
            bytes: captured.to_vec(),
            patches: vec![BodyOffsetPatch {
                body_offset: self.lazy_seek_field_pos,
                target_offset_within_body: target,
            }],
        })
    }
}

impl DeserializeUnrealObject for StaticMesh {
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
        let span = span!(Level::DEBUG, "deserialize_static_mesh");
        let _enter = span.enter();

        if runtime.game == crate::de::Game::SplinterCellPrototype {
            return self.deserialize_prototype::<E, _>(runtime, linker, reader);
        }

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let file_version = linker.borrow().version();
        let licensee_version = linker.borrow().licensee_version();

        if file_version < 0x5C {
            self.material_a = reader.read_object::<E>(runtime, linker)?;
            self.material_b = reader.read_object::<E>(runtime, linker)?;
        }

        if licensee_version >= 0x10 {
            // GIsSavegame guard skipped at normal load time.
            let bounds_count = reader.read_packed_int()?;
            assert!(bounds_count >= 0, "negative bounds count");
            let mut vertices = vec![0u8; (bounds_count as usize) * 0x20];
            if !vertices.is_empty() {
                reader.cheat(&mut vertices)?;
            }
            self.vertex_stream_vertices = vertices;

            self.vertex_stream_revision = reader.read_u32::<E>()?;

            self.index_buffer_a = read_raw_index_buffer::<E, _>(reader)?;
            self.index_buffer_b = read_raw_index_buffer::<E, _>(reader)?;
        }

        let section_count = reader.read_packed_int()?;
        assert!(section_count >= 0, "negative section count");
        self.sections = (0..section_count)
            .map(|_| -> io::Result<FStaticMeshSection> {
                let mut raw = [0u8; 18];
                reader.cheat(&mut raw)?;
                let object = reader.read_object::<E>(runtime, linker)?;
                Ok(FStaticMeshSection { object, raw })
            })
            .collect::<io::Result<Vec<_>>>()?;

        reader.cheat(&mut self.field_2c)?;
        reader.cheat(&mut self.field_38)?;
        self.field_44 = reader.read_u8()?;

        if file_version >= 0x4A && licensee_version >= 0xE {
            let count_70 = reader.read_packed_int()?;
            assert!(count_70 >= 0, "negative +0x70 count");
            self.collision_primitives = (0..count_70)
                .map(|_| -> io::Result<FStaticMeshCollisionPrimitive> {
                    let n = reader.read_packed_int()?;
                    assert!(n >= 0, "negative INT16 array len");
                    let mut indices = vec![0u8; (n as usize) * 2];
                    if !indices.is_empty() {
                        reader.cheat(&mut indices)?;
                    }
                    let tail_len = if n == 0 { 8 } else { 25 };
                    let mut tail = vec![0u8; tail_len];
                    reader.cheat(&mut tail)?;
                    Ok(FStaticMeshCollisionPrimitive { indices, tail })
                })
                .collect::<io::Result<Vec<_>>>()?;

            let count_7c = reader.read_packed_int()?;
            assert!(count_7c >= 0, "negative +0x7c count");
            let mut buf_7c = vec![0u8; (count_7c as usize) * 12];
            if !buf_7c.is_empty() {
                reader.cheat(&mut buf_7c)?;
            }
            self.collision_vertices = buf_7c;

            let count_88 = reader.read_packed_int()?;
            assert!(count_88 >= 0, "negative +0x88 count");
            self.collision_materials = (0..count_88)
                .map(|_| reader.read_object::<E>(runtime, linker))
                .collect::<io::Result<Vec<_>>>()?;
        }

        if file_version >= 0x61 {
            // `capture_len` BEFORE the read = body-relative byte
            // offset of the 4 LE bytes about to land in capture.
            // serialize emits a BodyOffsetPatch using this so ser.rs
            // can rewrite the stale absolute file offset on re-emit.
            self.lazy_seek_field_pos = reader.capture_len();
            self.lazy_seek_pos = reader.read_i32::<E>()?;
            reader.seek(SeekFrom::Start(self.lazy_seek_pos as u64))?;
        }

        if file_version >= 0x51 {
            self.internal_version = reader.read_u32::<E>()?;
        } else {
            self.internal_version = 0xFFFFFFFF;
        }
        // Signed compare matches `cmp` + `jge` in `sub_104d4010`. With u32 a
        // legacy default of 0xFFFFFFFF would falsely trigger the payload read.
        if (self.internal_version as i32) >= 8 {
            self.internal_version_payload = reader.read_u32::<E>()?;
        }

        debug!(
            "StaticMesh: {} sections, {} bound elems, {} +0x88 refs, lazy_seek_pos={:#x}, internal_version={:#x}",
            self.sections.len(),
            self.vertex_stream_vertices.len() / 0x20,
            self.collision_materials.len(),
            self.lazy_seek_pos,
            self.internal_version,
        );

        Ok(())
    }
}

impl StaticMesh {
    fn deserialize_prototype<E, R>(
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
        let licensee_version = linker.borrow().licensee_version();

        if licensee_version >= 0x10 {
            let bounds_count = read_packed_nonnegative(reader, "proto StaticMesh +0xc0")?;
            self.vertex_stream_vertices = read_static_mesh_vertices(reader, bounds_count)?;

            self.vertex_stream_revision = reader.read_u32::<E>()?;
            self.index_buffer_a = read_raw_index_buffer_recorded::<E, _>(reader)?;
            self.index_buffer_b = read_raw_index_buffer_recorded::<E, _>(reader)?;
        }

        let section_count = read_packed_nonnegative(reader, "proto StaticMesh section")?;
        self.sections = (0..section_count)
            .map(|_| -> io::Result<FStaticMeshSection> {
                let raw = read_section_raw(reader)?;
                let object = reader.read_object::<E>(runtime, linker)?;
                Ok(FStaticMeshSection { object, raw })
            })
            .collect::<io::Result<Vec<_>>>()?;

        read_chunks(reader, &mut self.field_2c, 4)?;
        read_chunks(reader, &mut self.field_38, 4)?;
        self.field_44 = reader.read_u8()?;

        if licensee_version >= 0xE {
            let collision_count =
                read_packed_nonnegative(reader, "proto StaticMesh collision entry")?;
            self.collision_primitives = (0..collision_count)
                .map(|_| read_static_mesh_collision_primitive_recorded(reader))
                .collect::<io::Result<Vec<_>>>()?;

            let vector_count = read_packed_nonnegative(reader, "proto StaticMesh FVector")?;
            let mut vectors = vec![0u8; vector_count * 12];
            if !vectors.is_empty() {
                read_chunks(reader, &mut vectors, 4)?;
            }
            self.collision_vertices = vectors;

            let object_count = read_packed_nonnegative(reader, "proto StaticMesh object ref")?;
            self.collision_materials = (0..object_count)
                .map(|_| reader.read_object::<E>(runtime, linker))
                .collect::<io::Result<Vec<_>>>()?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unsupported proto StaticMesh LicenseeVer {licensee_version:#x}; legacy collision branch is not implemented"
                ),
            ));
        }

        if file_version < 0x5C {
            let mut legacy = [0u8; 4];
            reader.read_exact(&mut legacy)?;
        }

        self.lazy_seek_field_pos = reader.capture_len();
        self.lazy_seek_pos = reader.read_i32::<E>()?;
        reader.seek(SeekFrom::Start(self.lazy_seek_pos as u64))?;
        let _ = reader.drain_alien_archive_reads()?;
        self.lazy_seek_target_capture_pos = reader.capture_len();

        self.internal_version = reader.read_u32::<E>()?;
        self.internal_version_payload = reader.read_u32::<E>()?;

        debug!(
            "Proto StaticMesh: {} sections, {} +0xc0 elems, {} collision entries, {} object refs, lazy_seek_pos={:#x}, internal_version={:#x}, payload={:#x}",
            self.sections.len(),
            self.vertex_stream_vertices.len() / 0x20,
            self.collision_primitives.len(),
            self.collision_materials.len(),
            self.lazy_seek_pos,
            self.internal_version,
            self.internal_version_payload,
        );

        Ok(())
    }
}
