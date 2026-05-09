use std::io;

use byteorder::ByteOrder;

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, utexture::Texture},
    reader::LinRead,
    runtime::UnrealRuntime,
};

/// `UCubemap::Serialize` resolves to `UTexture::Serialize` via vtable
/// slot 10 (`Engine_demo` UCubemap vtable at `0x10535cb8 + 0x28` =
/// `0x103067b2`). UCubemap has no native Serialize override of its
/// own; the six face textures are carried as tagged-property
/// references inside `UObject::Serialize`'s loop, and the trailing
/// `TArray<FMipmap>` (typically empty for a face-only cubemap) is
/// what `UTexture::Serialize` reads after the property loop.
///
/// At SC's archive constants (`Ver=0x64`, `LicenseeVer=0x11`,
/// `IsLoading=1`, `IsPersistent=1`, `GIsSavegame=0`,
/// `GUglyHackFlags & 8 = 0`), this resolves to:
///   1. `UMaterial::Serialize` (`sub_10306064`) -> `UObject::Serialize`
///      (`[0x10688b94]`) for the property tag-loop.
///   2. `sub_104f9b60`: read `TArray<FMipmap>` (`packed_int` count +
///      per-element FMipmap body). For face-only cubemaps the count
///      is 0, which still consumes one packed_int byte from the
///      stream (the `Ar << UObject*` mip body is skipped).
///
/// Without this delegation, Cubemap exports fall through to
/// `UObject::Serialize` only and leave the trailing 1-byte mip count
/// as a captured-bytes cheat (observed for
/// `generic_shaders.effect_files.Specularcubemap` in `1_2_1DefenseMinistry`).
#[derive(Default, Debug)]
pub struct Cubemap {
    pub parent_object: Texture,
}

impl DeserializeUnrealObject for Cubemap {
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
