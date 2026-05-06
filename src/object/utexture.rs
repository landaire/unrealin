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

/// SC's `UTexture::Serialize` (Engine_demo 0x103067b2, 0x104f9890):
///   - `Super::Serialize` (UMaterial, tagged-property loop only).
///   - Three `TArray<FMipmap>` serializes for the mip levels (we collapse
///     these into a single `mips` vector since SC's three calls write
///     the same array three times. See 0x104f9b60).
///
/// Each `FMipmap::operator<<` (0x104fe2b0) reads:
///   - `TLazyArray<BYTE> DataArray`.
///   - `INT USize`, `INT VSize`.
///   - `BYTE UBits`, `BYTE VBits`.
///
/// `TLazyArray<BYTE>::operator<<` (UT2004's `UnTemplate.h` matches
/// SC's 0x103cf0b0) does, on the loading path:
///   - `Ar << SeekPos` (4 bytes)
///   - `Ar.AttachLazyLoader(this)`. Internally `SavedPos = Ar.Tell()`.
///   - if `!GLazyLoad`: `this->Load()`, which does
///     `PushedPos = Ar.Tell(); Ar.Seek(SavedPos); Ar << TArray; Ar.Seek(PushedPos);`.
///     The `Ar.Seek(SavedPos)` is a no-op (we just set SavedPos=Tell)
///     but the QEMU plugin still records the `fseek`.
///   - `Ar.Seek(SeekPos)`. Moves to the post-data position.
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
                // `AttachLazyLoader` records `SavedPos = Ar.Tell()` and
                // `Load()` then does `Ar.Seek(SavedPos)`, a no-op move, but
                // QEMU records the fseek so we must emit the seek too.
                reader.seek(SeekFrom::Start(saved_pos))?;

                let data_len = reader.read_packed_int()?;
                assert!(data_len >= 0, "negative DataArray length");
                let mut data = vec![0u8; data_len as usize];
                if data_len > 0 {
                    reader.cheat(&mut data)?;
                }

                // `Load()` ends with `Ar.Seek(PushedPos)` (back to before the
                // array was read); then `TLazyArray::operator<<` does
                // `Ar.Seek(SeekPos)` to the post-skip-block position.
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
