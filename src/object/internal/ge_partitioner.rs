use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::reader::LinRead;
use crate::reader::UnrealReadExt;

/// `GEPartitioner` is a SC-specific spatial structure for "Geometric
/// Events" (gore decals, footprints, etc.) and is serialized at
/// `ULevel + 0x3914` whenever `LicenseeVer > 0` (always at SC=0x11).
/// Mirrors the xbe `sub_150ff0` operator<<.
///
/// Disk layout (Ar.IsLoading=1):
/// - 7x raw u32 fields at struct offsets `+0`, `+0x20`, `+0x24`,
///   `+0x28`, `+0x2c`, `+0x30`, `+0x34`.
/// - `TArray<GEObject*>` at `+0x14`: packed_int count + per-element
///   polymorphic dispatch via xbe `sub_150710`.
/// - `TArray<int>` at `+0x8`: packed_int count + 4 raw bytes per
///   element (each is an index into the `GEObject*` array, or `-1`).
/// - Polymorphic `GERootNode` at `+0x4` via xbe `sub_1512b0`. The
///   node is a binary BSP-style tree: tags 1/2/4 are interior nodes
///   (4B + 2 recursive children, all sharing vftable[4]=`sub_1514d0`)
///   and tag 8 is a leaf (2 raw u32 indices via `sub_151130`). Tag
///   3, 5-7, 9-15 produce no body. Tag 16 has a fifth jump-table
///   slot whose handler hasn't been REd yet.
#[derive(Default, Debug, Clone)]
pub struct GEPartitioner {
    pub dword_0: u32,
    pub dword_20: u32,
    pub dword_24: u32,
    pub dword_28: u32,
    pub dword_2c: u32,
    pub dword_30: u32,
    pub dword_34: u32,
    pub objects: Vec<GEObject>,
    pub indices: Vec<u32>,
    pub root: GERootNode,
}

/// One element of `GEPartitioner.objects`. Mirrors xbe `sub_150710`
/// (per-element op<<) plus its common tail at `0x150851`:
///   1. read u32 tag
///   2. for tags {1,2,4,8,16,32,64}: alloc subclass, read u32 at +8,
///      then dispatch into subclass vtable[7] for the body
///   3. for tag = 0 or tag > 64 (`tag-1 u> 0x3f`): early return at
///      `0x150868` -- no body, no `+8` read
///   4. for in-range tags that hit lookup_table_150890[tag-1] = 7
///      (i.e. 3, 5..7, 9..15, 17..31, 33..63): same early return --
///      jump_table_150870[7] is `0x150868`
///
/// Tag -> subclass body reader (vtable[7]):
/// - 1, 2, 4, 8, 64 -> `sub_1502d0`: 6x u32 at obj+{0xc..0x20}
/// - 16, 32       -> `sub_150540`: 9x u32 at obj+{0xc..0x2c}
#[derive(Debug, Clone)]
pub struct GEObject {
    pub tag: u32,
    pub body: GEObjectBody,
}

#[derive(Debug, Clone)]
pub enum GEObjectBody {
    /// Tag 0 or tag > 64, or any in-range tag whose
    /// lookup_table_150890 index is 7 (3, 5..7, 9..15, 17..31, 33..63).
    /// No body bytes follow the tag.
    Empty,
    /// Tags 1, 2, 4, 8, 64. Body: u32 at +8 + 6x u32 at +0xc..+0x20.
    Small { field_8: u32, body: [u32; 6] },
    /// Tags 16, 32. Body: u32 at +8 + 9x u32 at +0xc..+0x2c.
    Large { field_8: u32, body: [u32; 9] },
}

/// One node of the `GEPartitioner.root` BSP-style tree. The tag is
/// always serialized; the body shape is determined by the tag.
#[derive(Debug, Clone)]
pub struct GERootNode {
    pub tag: u32,
    pub body: GERootBody,
}

#[derive(Debug, Clone)]
pub enum GERootBody {
    /// `tag == 0` or `tag` outside `[1, 16]` -- no body bytes follow.
    Empty,
    /// Interior node: tags 1, 3, 7. xbe vftable[4] = `sub_1514d0`:
    /// 4 raw bytes, then two recursive polymorphic node reads.
    Interior {
        dword_c: u32,
        left: Box<GERootNode>,
        right: Box<GERootNode>,
    },
    /// Leaf node: tag 15. xbe vftable[4] = `sub_151130`: two raw
    /// u32 indices into `GEPartitioner.objects` (or `-1` for none).
    Leaf { idx_a: u32, idx_b: u32 },
}

impl Default for GERootNode {
    fn default() -> Self {
        GERootNode {
            tag: 0,
            body: GERootBody::Empty,
        }
    }
}

pub fn read_ge_partitioner<E, R>(reader: &mut R) -> io::Result<GEPartitioner>
where
    E: ByteOrder,
    R: LinRead,
{
    let dword_0 = reader.read_u32::<E>()?;
    let dword_20 = reader.read_u32::<E>()?;
    let dword_24 = reader.read_u32::<E>()?;
    let dword_28 = reader.read_u32::<E>()?;
    let dword_2c = reader.read_u32::<E>()?;
    let dword_30 = reader.read_u32::<E>()?;
    let dword_34 = reader.read_u32::<E>()?;

    let object_count = reader.read_packed_int()?;
    // Negative TArray counts cannot occur in an engine-written body
    // (FArray operator<< always writes non-negative ARCount). A negative
    // value here means the level body is misaligned with the engine's
    // read order -- see the matching FURL/TravelInfo comments in
    // `ulevel_base.rs` / `ulevel.rs`. Abort cleanly so callers unwind
    // without panicking.
    if object_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GEPartitioner object count negative ({object_count}) -- body is misaligned"),
        ));
    }
    let mut objects = Vec::with_capacity(object_count as usize);
    for _ in 0..object_count {
        objects.push(read_ge_object::<E, _>(reader)?);
    }

    let index_count = reader.read_packed_int()?;
    if index_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("GEPartitioner index count negative ({index_count}) -- body is misaligned"),
        ));
    }
    let mut indices = Vec::with_capacity(index_count as usize);
    for _ in 0..index_count {
        indices.push(reader.read_u32::<E>()?);
    }

    let root = read_ge_root_node::<E, _>(reader)?;

    Ok(GEPartitioner {
        dword_0,
        dword_20,
        dword_24,
        dword_28,
        dword_2c,
        dword_30,
        dword_34,
        objects,
        indices,
        root,
    })
}

fn read_ge_root_node<E, R>(reader: &mut R) -> io::Result<GERootNode>
where
    E: ByteOrder,
    R: LinRead,
{
    let tag = reader.read_u32::<E>()?;
    // xbe `sub_1512b0` (the polymorphic op<<):
    //   1. read u32 tag
    //   2. if tag-1 u> 0xf: return -- no body
    //   3. else lookup_table_15143c[tag-1] gives jump_table_151424 idx;
    //      idx 0 (tag 1) and idx 5 (any "default" in-range tag) both
    //      jump to 0x15141b which is the return block (no body)
    //
    // Real handlers: tags 2, 4, 8 -> vtable 0x281600/0x281614/0x281628,
    // each with vtable[4] = sub_1514d0 (interior: 4B + two recursive
    // sub_1512b0 calls). Tag 16 -> vtable 0x2815d8, vtable[4] =
    // sub_151130 (leaf: two u32 reads via sub_150b70). Tag 1 is the
    // "null marker" the save path writes when `*arg2 == nullptr`.
    let body = match tag {
        2 | 4 | 8 => {
            let dword_c = reader.read_u32::<E>()?;
            let left = Box::new(read_ge_root_node::<E, _>(reader)?);
            let right = Box::new(read_ge_root_node::<E, _>(reader)?);
            GERootBody::Interior {
                dword_c,
                left,
                right,
            }
        }
        16 => {
            let idx_a = reader.read_u32::<E>()?;
            let idx_b = reader.read_u32::<E>()?;
            GERootBody::Leaf { idx_a, idx_b }
        }
        _ => GERootBody::Empty,
    };
    Ok(GERootNode { tag, body })
}

fn read_ge_object<E, R>(reader: &mut R) -> io::Result<GEObject>
where
    E: ByteOrder,
    R: LinRead,
{
    let tag = reader.read_u32::<E>()?;
    // xbe `sub_150710`: `(tag - 1) u> 0x3f` short-circuits to the
    // return block at 0x150868 -- no body bytes. Catches tag = 0 and
    // tag > 64.
    if tag == 0 || tag > 64 {
        return Ok(GEObject {
            tag,
            body: GEObjectBody::Empty,
        });
    }
    // For in-range tags, lookup_table_150890[tag-1] gives the
    // jump_table_150870 index. Index 7 is the same return block; only
    // indices 0..6 actually allocate + read body. The valid tags are
    // {1, 2, 4, 8, 16, 32, 64}.
    let body = match tag {
        1 | 2 | 4 | 8 | 64 => {
            // sub_1502d0: read 4 + 24 = 28 bytes (u32 at +8 + 6x u32 at +0xc..+0x20).
            let field_8 = reader.read_u32::<E>()?;
            let mut body = [0u32; 6];
            for slot in body.iter_mut() {
                *slot = reader.read_u32::<E>()?;
            }
            GEObjectBody::Small { field_8, body }
        }
        16 | 32 => {
            // sub_150540: read 4 + 36 = 40 bytes (u32 at +8 + 9x u32 at +0xc..+0x2c).
            let field_8 = reader.read_u32::<E>()?;
            let mut body = [0u32; 9];
            for slot in body.iter_mut() {
                *slot = reader.read_u32::<E>()?;
            }
            GEObjectBody::Large { field_8, body }
        }
        // Tags 3, 5..7, 9..15, 17..31, 33..63: lookup_table_150890
        // returns 7 -> return block, no body.
        _ => GEObjectBody::Empty,
    };
    Ok(GEObject { tag, body })
}
