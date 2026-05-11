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

/// Mirrors `UModel::Serialize` (Engine_demo `sub_10420a80`).
///
/// SC archive constants: `Ver=0x64` (100), `LicenseeVer=0x11`,
/// `IsLoading=1`, `IsTrans=0`. Layouts are taken from the per-element
/// SC operator<< implementations below; UT2004 source is similar but
/// SC has shifted FBspNode field offsets (NumVertices is at +0x57
/// instead of grouped with iZone/iLeaf) and FBspNode/FBspSurf/FVert
/// disk forms encode AR_INDEX as `FCompactIndex` operator<<.
#[derive(Default, Debug)]
pub struct Model {
    pub parent_object: Primitive,
    /// `TArray<FVector>` Vectors at `+0x7c`, 12 bytes/elt raw.
    pub vectors: Vec<u8>,
    /// `TArray<FVector>` Points at `+0x8c`, 12 bytes/elt raw.
    pub points: Vec<u8>,
    pub nodes: Vec<FBspNode>,
    pub surfs: Vec<FBspSurf>,
    pub verts: Vec<FVert>,
    pub num_shared_sides: u32,
    pub num_zones: u32,
    pub zones: Vec<FZoneProperties>,
    pub polys: Option<RcUnrealObject>,
    pub light_map_index: Vec<FLightMapIndex>,
    pub light_bitmaps: Vec<FLightBitmap>,
    /// `TArray<FBox>` at `+0xc4`, 25 bytes/elt raw (6x f32 + 1 byte
    /// IsValid). Stored as raw bytes since the layout is fixed.
    pub bounds: Vec<u8>,
    /// `TArray<INT>` LeafHulls at `+0xd0`, 4 bytes/elt raw.
    pub leaf_hulls: Vec<u8>,
    pub leaves: Vec<FLeaf>,
    pub lights: Vec<Option<RcUnrealObject>>,
    pub root_outside: u32,
    pub linked: u32,
    pub vertex_streams: Vec<FBspVertexStream>,
}

/// FBspNode disk layout per `sub_10420720`. Fields named after the SC
/// memory offsets they originate from since UT2004's source struct
/// only loosely matches.
#[derive(Default, Debug, Clone)]
pub struct FBspNode {
    /// FPlane at `+0..+0x0F`, four 32-bit floats raw.
    pub plane: [u8; 16],
    /// QWORD at `+0x10`, eight bytes raw.
    pub zone_mask: [u8; 8],
    /// BYTE NumVertices at `+0x57`.
    pub num_vertices: u8,
    pub i_vert_pool: i32,
    pub i_surf: i32,
    pub i_child: [i32; 3],
    pub i_collision_bound: i32,
    pub i_render_bound: i32,
    /// FSphere ExclusiveSphereBound at `+0x2c`: FVector(12B) + radius(4B).
    pub exclusive_sphere_bound: [u8; 16],
    /// FSphere InclusiveSphereBound at `+0x3c`.
    pub inclusive_sphere_bound: [u8; 16],
    pub byte_54: u8,
    pub byte_55: u8,
    pub byte_56: u8,
    pub dword_58: u32,
    pub dword_5c: u32,
    /// 4B at `+0x64`, gated `Ver >= 0x5d` (fires at SC=100).
    pub dword_64: u32,
    /// 4B at `+0x60`, gated `Ver >= 0x5c` (fires at SC=100).
    pub dword_60: u32,
}

fn read_packed_u8s<R>(reader: &mut R, stride: usize) -> io::Result<Vec<u8>>
where
    R: LinRead,
{
    let count = reader.read_packed_int()?;
    assert!(count >= 0, "UModel TArray count negative");
    let total = (count as usize) * stride;
    let mut buf = vec![0u8; total];
    if !buf.is_empty() {
        reader.cheat(&mut buf)?;
    }
    Ok(buf)
}

fn read_bsp_node<R>(reader: &mut R) -> io::Result<FBspNode>
where
    R: LinRead,
{
    let mut node = FBspNode::default();
    let mut head = [0u8; 16 + 8 + 1];
    reader.cheat(&mut head)?;
    node.plane.copy_from_slice(&head[..16]);
    node.zone_mask.copy_from_slice(&head[16..24]);
    node.num_vertices = head[24];
    node.i_vert_pool = reader.read_packed_int()?;
    node.i_surf = reader.read_packed_int()?;
    node.i_child[0] = reader.read_packed_int()?;
    node.i_child[1] = reader.read_packed_int()?;
    node.i_child[2] = reader.read_packed_int()?;
    node.i_collision_bound = reader.read_packed_int()?;
    node.i_render_bound = reader.read_packed_int()?;
    reader.cheat(&mut node.exclusive_sphere_bound)?;
    reader.cheat(&mut node.inclusive_sphere_bound)?;
    let mut tail = [0u8; 1 + 1 + 1 + 4 + 4 + 4 + 4];
    reader.cheat(&mut tail)?;
    node.byte_54 = tail[0];
    node.byte_55 = tail[1];
    node.byte_56 = tail[2];
    node.dword_58 = u32::from_le_bytes(tail[3..7].try_into().unwrap());
    node.dword_5c = u32::from_le_bytes(tail[7..11].try_into().unwrap());
    node.dword_64 = u32::from_le_bytes(tail[11..15].try_into().unwrap());
    node.dword_60 = u32::from_le_bytes(tail[15..19].try_into().unwrap());
    Ok(node)
}

/// FBspSurf disk layout per `sub_104201a0`.
#[derive(Default, Debug, Clone)]
pub struct FBspSurf {
    pub material: Option<RcUnrealObject>,
    pub poly_flags: u32,
    pub p_base: i32,
    pub v_normal: i32,
    pub v_texture_u: i32,
    pub v_texture_v: i32,
    pub i_light_map: i32,
    pub i_brush_poly: i32,
    pub actor: Option<RcUnrealObject>,
    pub plane: [u8; 16],
}

fn read_bsp_surf<E, R>(
    reader: &mut R,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
) -> io::Result<FBspSurf>
where
    E: ByteOrder,
    R: LinRead,
{
    let material = reader.read_object::<E>(runtime, linker)?;
    let poly_flags = reader.read_u32::<E>()?;
    let p_base = reader.read_packed_int()?;
    let v_normal = reader.read_packed_int()?;
    let v_texture_u = reader.read_packed_int()?;
    let v_texture_v = reader.read_packed_int()?;
    let i_light_map = reader.read_packed_int()?;
    let i_brush_poly = reader.read_packed_int()?;
    let actor = reader.read_object::<E>(runtime, linker)?;
    let mut plane = [0u8; 16];
    reader.cheat(&mut plane)?;
    Ok(FBspSurf {
        material,
        poly_flags,
        p_base,
        v_normal,
        v_texture_u,
        v_texture_v,
        i_light_map,
        i_brush_poly,
        actor,
        plane,
    })
}

/// FVert disk layout per `sub_103a44e0`: two `FCompactIndex` packed
/// ints. UT2004's FVert is in-memory 8 bytes; on disk the AR_INDEX
/// encoding makes it variable-length.
#[derive(Default, Debug, Clone)]
pub struct FVert {
    pub p_vertex: i32,
    pub i_side: i32,
}

fn read_vert<R>(reader: &mut R) -> io::Result<FVert>
where
    R: LinRead,
{
    Ok(FVert {
        p_vertex: reader.read_packed_int()?,
        i_side: reader.read_packed_int()?,
    })
}

/// FZoneProperties disk layout per `sub_10427d20`. The serializer
/// reads `+0x08` and `+0x10` *before* `+0x04`, so disk order is
/// `(actor, qword_8, qword_10, dword_4)` even though in-memory order
/// would put `dword_4` before the qwords.
#[derive(Default, Debug, Clone)]
pub struct FZoneProperties {
    pub zone_actor: Option<RcUnrealObject>,
    pub qword_8: [u8; 8],
    pub qword_10: [u8; 8],
    pub dword_4: u32,
}

fn read_zone<E, R>(
    reader: &mut R,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
) -> io::Result<FZoneProperties>
where
    E: ByteOrder,
    R: LinRead,
{
    let zone_actor = reader.read_object::<E>(runtime, linker)?;
    let mut tail = [0u8; 8 + 8 + 4];
    reader.cheat(&mut tail)?;
    let mut qword_8 = [0u8; 8];
    let mut qword_10 = [0u8; 8];
    qword_8.copy_from_slice(&tail[..8]);
    qword_10.copy_from_slice(&tail[8..16]);
    let dword_4 = u32::from_le_bytes(tail[16..20].try_into().unwrap());
    Ok(FZoneProperties {
        zone_actor,
        qword_8,
        qword_10,
        dword_4,
    })
}

/// FLeaf disk layout per `sub_10427df0`: 3 packed_ints + 8B raw tail.
#[derive(Default, Debug, Clone)]
pub struct FLeaf {
    pub i_zone: i32,
    pub i_perm: i32,
    pub i_volumetric: i32,
    pub trailing: [u8; 8],
}

fn read_leaf<R>(reader: &mut R) -> io::Result<FLeaf>
where
    R: LinRead,
{
    let i_zone = reader.read_packed_int()?;
    let i_perm = reader.read_packed_int()?;
    let i_volumetric = reader.read_packed_int()?;
    let mut trailing = [0u8; 8];
    reader.cheat(&mut trailing)?;
    Ok(FLeaf {
        i_zone,
        i_perm,
        i_volumetric,
        trailing,
    })
}

/// FLightBitmap disk layout per `sub_104270e0` per-element body.
#[derive(Default, Debug, Clone)]
pub struct FLightBitmap {
    pub light: Option<RcUnrealObject>,
    pub data: Vec<u8>,
    pub dword_c: u32,
    pub dword_10: u32,
    /// 4B at `+0x14`, gated `Ver >= 0x5b` (fires at SC=100).
    pub dword_14: u32,
}

fn read_light_bitmap<E, R>(
    reader: &mut R,
    runtime: &mut UnrealRuntime,
    linker: &RcLinker,
) -> io::Result<FLightBitmap>
where
    E: ByteOrder,
    R: LinRead,
{
    let light = reader.read_object::<E>(runtime, linker)?;
    let count = reader.read_packed_int()?;
    assert!(count >= 0, "FLightBitmap byte count negative");
    let mut data = vec![0u8; count as usize];
    if !data.is_empty() {
        reader.cheat(&mut data)?;
    }
    let mut tail = [0u8; 4 + 4 + 4];
    reader.cheat(&mut tail)?;
    Ok(FLightBitmap {
        light,
        data,
        dword_c: u32::from_le_bytes(tail[0..4].try_into().unwrap()),
        dword_10: u32::from_le_bytes(tail[4..8].try_into().unwrap()),
        dword_14: u32::from_le_bytes(tail[8..12].try_into().unwrap()),
    })
}

/// FLightMapIndex disk layout per `sub_10427ed0` (Ver >= 0x5c primary
/// path, fires at SC=100). Two large raw blocks bracket two trailing
/// bytes, then 9 floats (Ver >= 0x60), then four `FCompactIndex`
/// packed ints.
#[derive(Default, Debug, Clone)]
pub struct FLightMapIndex {
    /// 18 raw f32 reads at `+0..+0x47` (matrix-shaped data, 72 bytes).
    pub block_a: Vec<u8>,
    /// 16 raw f32 reads at `+0x48..+0x87` (64 bytes).
    pub block_b: Vec<u8>,
    pub byte_ac: u8,
    pub byte_ad: u8,
    /// 9 f32 raw at `+0x88..+0xab`, gated `Ver >= 0x60` (fires at SC).
    pub nine_floats: Vec<u8>,
    pub packed_b0: i32,
    pub packed_b4: i32,
    /// Packed int at `+0xb8`, gated `Ver >= 0x5c` (fires at SC).
    pub packed_b8: i32,
    /// Packed int at `+0xbc`, gated `Ver >= 0x5c` (fires at SC).
    pub packed_bc: i32,
}

fn read_light_map_index<R>(reader: &mut R) -> io::Result<FLightMapIndex>
where
    R: LinRead,
{
    let mut block_a = vec![0u8; 72];
    reader.cheat(&mut block_a)?;
    let mut block_b = vec![0u8; 64];
    reader.cheat(&mut block_b)?;
    let mut two = [0u8; 2];
    reader.cheat(&mut two)?;
    let mut nine_floats = vec![0u8; 36];
    reader.cheat(&mut nine_floats)?;
    let packed_b0 = reader.read_packed_int()?;
    let packed_b4 = reader.read_packed_int()?;
    let packed_b8 = reader.read_packed_int()?;
    let packed_bc = reader.read_packed_int()?;
    Ok(FLightMapIndex {
        block_a,
        block_b,
        byte_ac: two[0],
        byte_ad: two[1],
        nine_floats,
        packed_b0,
        packed_b4,
        packed_b8,
        packed_bc,
    })
}

/// FBspVertexStream disk layout per `sub_10427970` per-element body.
/// The nested TArray has 28-byte raw elements (7 raw f32 each).
#[derive(Default, Debug, Clone)]
pub struct FBspVertexStream {
    pub inner: Vec<u8>,
    pub dword_18: u32,
}

fn read_vertex_stream<R>(reader: &mut R) -> io::Result<FBspVertexStream>
where
    R: LinRead,
{
    let inner_count = reader.read_packed_int()?;
    assert!(inner_count >= 0, "FBspVertexStream inner count negative");
    let total = (inner_count as usize) * 28;
    let mut inner = vec![0u8; total];
    if !inner.is_empty() {
        reader.cheat(&mut inner)?;
    }
    let mut tail = [0u8; 4];
    reader.cheat(&mut tail)?;
    Ok(FBspVertexStream {
        inner,
        dword_18: u32::from_le_bytes(tail),
    })
}

impl DeserializeUnrealObject for Model {
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
        // Step 0: UPrimitive::Serialize (UObject tag loop + BBox/Sphere).
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        // Step 1-2: Vectors, Points -- TArray<FVector>, 12B/elt raw.
        self.vectors = read_packed_u8s(reader, 12)?;
        self.points = read_packed_u8s(reader, 12)?;
        // Step 3: Nodes -- TArray<FBspNode>.
        let nodes_count = reader.read_packed_int()?;
        assert!(nodes_count >= 0, "Nodes count negative");
        self.nodes = Vec::with_capacity(nodes_count as usize);
        for _ in 0..nodes_count {
            self.nodes.push(read_bsp_node(reader)?);
        }
        // Step 4: Surfs -- TArray<FBspSurf>.
        let surfs_count = reader.read_packed_int()?;
        assert!(surfs_count >= 0, "Surfs count negative");
        self.surfs = Vec::with_capacity(surfs_count as usize);
        for _ in 0..surfs_count {
            self.surfs
                .push(read_bsp_surf::<E, _>(reader, runtime, linker)?);
        }
        // Step 5: Verts -- TArray<FVert>, 2 packed_ints/elt.
        let verts_count = reader.read_packed_int()?;
        assert!(verts_count >= 0, "Verts count negative");
        self.verts = Vec::with_capacity(verts_count as usize);
        for _ in 0..verts_count {
            self.verts.push(read_vert(reader)?);
        }
        // Step 6: NumSharedSides (raw u32).
        self.num_shared_sides = reader.read_u32::<E>()?;
        // Step 7: NumZones (raw u32).
        self.num_zones = reader.read_u32::<E>()?;
        // Step 8: Zones[NumZones] inline array.
        self.zones = Vec::with_capacity(self.num_zones as usize);
        for _ in 0..self.num_zones {
            self.zones.push(read_zone::<E, _>(reader, runtime, linker)?);
        }
        // Step 9: Polys (Ar << UObject*) + Preload.
        self.polys = reader.read_object::<E>(runtime, linker)?;
        if let Some(polys) = self.polys.clone() {
            runtime.full_load_object::<E, _>(&polys, reader)?;
        }
        // Step 10: LightMapIndex (Ver >= 0x5c branch fires at SC=100).
        let lightmap_count = reader.read_packed_int()?;
        assert!(lightmap_count >= 0, "LightMapIndex count negative");
        self.light_map_index = Vec::with_capacity(lightmap_count as usize);
        for _ in 0..lightmap_count {
            self.light_map_index.push(read_light_map_index(reader)?);
        }
        // Step 11: LightBitmaps.
        let bitmaps_count = reader.read_packed_int()?;
        assert!(bitmaps_count >= 0, "LightBitmaps count negative");
        self.light_bitmaps = Vec::with_capacity(bitmaps_count as usize);
        for _ in 0..bitmaps_count {
            self.light_bitmaps
                .push(read_light_bitmap::<E, _>(reader, runtime, linker)?);
        }
        // Step 12: Bounds -- TArray<FBox>, 25B/elt raw (6x 4B + 1B).
        self.bounds = read_packed_u8s(reader, 25)?;
        // Step 13: LeafHulls -- TArray<INT>, 4B/elt raw.
        self.leaf_hulls = read_packed_u8s(reader, 4)?;
        // Step 14: Leaves -- TArray<FLeaf>.
        let leaves_count = reader.read_packed_int()?;
        assert!(leaves_count >= 0, "Leaves count negative");
        self.leaves = Vec::with_capacity(leaves_count as usize);
        for _ in 0..leaves_count {
            self.leaves.push(read_leaf(reader)?);
        }
        // Step 15: Lights -- TArray<UObject*>, packed_int per element.
        let lights_count = reader.read_packed_int()?;
        assert!(lights_count >= 0, "Lights count negative");
        self.lights = Vec::with_capacity(lights_count as usize);
        for _ in 0..lights_count {
            self.lights.push(reader.read_object::<E>(runtime, linker)?);
        }
        // Step 16-17: RootOutside (UBOOL=4B), Linked (UBOOL=4B).
        self.root_outside = reader.read_u32::<E>()?;
        self.linked = reader.read_u32::<E>()?;
        // Step 18: VertexStreams (Ver >= 0x5d branch fires at SC=100).
        let vstream_count = reader.read_packed_int()?;
        assert!(vstream_count >= 0, "VertexStreams count negative");
        self.vertex_streams = Vec::with_capacity(vstream_count as usize);
        for _ in 0..vstream_count {
            self.vertex_streams.push(read_vertex_stream(reader)?);
        }

        Ok(())
    }
}
