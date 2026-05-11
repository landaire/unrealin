use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::usubsystem::Subsystem;
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

/// Rendering device for 3D graphics
#[derive(Debug, Default)]
pub struct RenderDevice {
    pub parent_object: Subsystem,
    pub decomp_format: u8,
    pub recommended_lod: i32,
    pub terrain_lod: u32,
    pub high_detail_actors: bool,
    pub super_high_detail_actors: bool,
    pub detail_textures: bool,
    pub use_compressed_lightmaps: bool,
    pub use_stencil: bool,
    pub use_16bit: bool,
    pub use_16bit_textures: bool,
    pub low_quality_terrain: bool,
    pub skybox_hack: bool,
}

impl DeserializeUnrealObject for RenderDevice {
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
            .deserialize::<E, R>(runtime, linker, reader)?;

        // Read the basic render device properties
        self.decomp_format = reader.read_u8()?;
        self.recommended_lod = reader.read_i32::<E>()?;
        self.terrain_lod = reader.read_u32::<E>()?;

        // Read bitfield flags
        let flags = reader.read_u32::<E>()?;
        self.high_detail_actors = (flags & 0x01) != 0;
        self.super_high_detail_actors = (flags & 0x02) != 0;
        self.detail_textures = (flags & 0x04) != 0;
        self.use_compressed_lightmaps = (flags & 0x08) != 0;
        self.use_stencil = (flags & 0x10) != 0;
        self.use_16bit = (flags & 0x20) != 0;
        self.use_16bit_textures = (flags & 0x40) != 0;
        self.low_quality_terrain = (flags & 0x80) != 0;
        self.skybox_hack = (flags & 0x100) != 0;

        Ok(())
    }
}
