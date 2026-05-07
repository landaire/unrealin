use std::io::{self, SeekFrom};

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, debug, span, trace};

use std::io::Seek;

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// SC's `UTexture::Serialize` (Engine_demo `0x103067b2` -> `0x104f9890`):
///   - `UMaterial::Serialize` -> `UObject::Serialize` (tagged-property loop).
///   - `sub_104f9b60` reads the mip array.
///
/// `sub_104f9b60` runs `*GLazyLoad = 1` then `if (*GUglyHackFlags & 8 != 0)
/// *GLazyLoad = 0`. SC's runtime has bit 0x08 of `GUglyHackFlags` set, so
/// `GLazyLoad = 0` for the inner `FMipmap::operator<<` calls, which means
/// `TLazyArray<BYTE>::operator<<` (`sub_103cf0b0`) takes the `if
/// (*GLazyLoad == 0) Load();` branch and runs the lazy body inline.
///
/// `FMipmap::operator<<` (`sub_104fe2b0`) per mip:
///   - `TLazyArray<BYTE> DataArray` (`sub_103cf0b0`):
///     - Read 4 bytes (`SeekPos`).
///     - `Ar.AttachLazyLoader(this)` (memory write, no IO).
///     - `Load()`:
///       - `PushedPos = Tell()`.
///       - `Ar.Seek(SavedPos)`: no-op since `SavedPos == Tell()`.
///       - `Ar << TArray<BYTE>`: read array length packed-int + bytes.
///       - `Ar.Seek(PushedPos)`: back to before the array.
///     - `Ar.Seek(SeekPos)`: final move past the lazy body.
///   - 4 bytes `USize`, 4 bytes `VSize`, 1 byte `UBits`, 1 byte `VBits`.
#[derive(Default, Debug)]
pub struct Texture {
    pub parent_object: Object,

    pub mips: Vec<Mipmap>,
}

#[derive(Default, Debug)]
pub struct Mipmap {
    pub skip_offset: i32,
    pub data: Vec<u8>,
    pub u_size: i32,
    pub v_size: i32,
    pub u_bits: u8,
    pub v_bits: u8,
}

impl DeserializeUnrealObject for Texture {
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
        let span = span!(Level::DEBUG, "deserialize_texture");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let mip_count = reader.read_packed_int()?;
        assert!(mip_count >= 0, "negative mip count");

        debug!("texture has {} mips", mip_count);

        self.mips = (0..mip_count)
            .map(|_| -> io::Result<Mipmap> {
                let skip_offset = reader.read_i32::<E>()?;
                let saved_pos = reader.stream_position()?;
                reader.seek(SeekFrom::Start(saved_pos))?;

                let data_len = reader.read_packed_int()?;
                assert!(data_len >= 0, "negative DataArray length");
                let mut data = vec![0u8; data_len as usize];
                if data_len > 0 {
                    reader.cheat(&mut data)?;
                }

                reader.seek(SeekFrom::Start(saved_pos))?;
                reader.seek(SeekFrom::Start(skip_offset as u64))?;

                Ok(Mipmap {
                    skip_offset,
                    data,
                    u_size: reader.read_i32::<E>()?,
                    v_size: reader.read_i32::<E>()?,
                    u_bits: reader.read_u8()?,
                    v_bits: reader.read_u8()?,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;

        trace!("read {} mips", self.mips.len());

        Ok(())
    }
}
