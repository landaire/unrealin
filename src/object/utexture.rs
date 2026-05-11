use std::io::SeekFrom;
use std::io::{
    self,
};

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;
use tracing::Level;
use tracing::debug;
use tracing::span;
use tracing::trace;

use std::io::Seek;

use crate::de::ExportIndex;
use crate::de::Linker;
use crate::de::RcLinker;
use crate::object::BodyKind;
use crate::object::BodyOffsetPatch;
use crate::object::DeserializeUnrealObject;
use crate::object::SerializeUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

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
    /// Body-relative byte offset of this mip's `skip_offset` field
    /// within the captured Texture body. `Texture::serialize` uses
    /// this to emit a `BodyOffsetPatch` so ser.rs can rewrite the
    /// stale absolute offset when the body lands at a new file
    /// position on re-emit.
    pub skip_offset_field_pos: usize,
}

impl SerializeUnrealObject for Texture {
    /// Patch each mipmap's stale absolute `SeekPos` to its new
    /// position. The body bytes themselves stay verbatim (we don't
    /// reconstruct mipmap data); only the 4-byte skip targets get
    /// rewritten when ser.rs places the body at its new file offset.
    fn serialize<E>(
        &self,
        linker: &Linker,
        export_index: ExportIndex,
        captured: &[u8],
    ) -> std::io::Result<BodyKind>
    where
        E: byteorder::ByteOrder,
    {
        if self.mips.is_empty() {
            return Ok(BodyKind::Opaque(captured.to_vec()));
        }
        let old_body_offset = linker
            .find_export_by_index(export_index)
            .map(|e| e.serial_offset())
            .unwrap_or(0);
        let mut patches = Vec::with_capacity(self.mips.len());
        for mip in &self.mips {
            // The on-disk skip_offset = absolute file offset of the
            // byte right after the mip's lazy `TArray<BYTE>` data
            // (the engine seeks there to skip past it). Subtract the
            // body's original file offset to get the body-relative
            // target, then ser.rs adds the new body offset on patch.
            let target = (mip.skip_offset as u64).saturating_sub(old_body_offset) as usize;
            patches.push(BodyOffsetPatch {
                body_offset: mip.skip_offset_field_pos,
                target_offset_within_body: target,
            });
        }
        Ok(BodyKind::Patched {
            bytes: captured.to_vec(),
            patches,
        })
    }
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
                // `capture_len` BEFORE the read = body-relative byte
                // offset where the skip_offset's 4 LE bytes will land.
                // Texture::serialize emits a BodyOffsetPatch using
                // this so ser.rs can rewrite the stale absolute file
                // offset when the body is placed at a new position.
                let skip_offset_field_pos = reader.capture_len();
                let skip_offset = reader.read_i32::<E>()?;
                let saved_pos = reader.stream_position()?;
                reader.seek(SeekFrom::Start(saved_pos))?;

                let data_len = reader.read_packed_int()?;
                assert!(data_len >= 0, "negative DataArray length");
                let mut data = vec![0u8; data_len as usize];
                // The engine's `Ar.Serialize(buf, data_len)` runs even
                // when data_len == 0; that emits a `Read(len=0)` syscall
                // which we must consume here. Skipping the call when
                // empty would leave the no-op trace event queued and
                // collide with the seek that immediately follows.
                reader.cheat(&mut data)?;

                reader.seek(SeekFrom::Start(saved_pos))?;
                reader.seek(SeekFrom::Start(skip_offset as u64))?;

                Ok(Mipmap {
                    skip_offset,
                    data,
                    u_size: reader.read_i32::<E>()?,
                    v_size: reader.read_i32::<E>()?,
                    u_bits: reader.read_u8()?,
                    v_bits: reader.read_u8()?,
                    skip_offset_field_pos,
                })
            })
            .collect::<io::Result<Vec<_>>>()?;

        trace!("read {} mips", self.mips.len());

        Ok(())
    }
}
