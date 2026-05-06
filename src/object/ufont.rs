use std::io;

use byteorder::{ByteOrder, ReadBytesExt};
use tracing::{Level, debug, span, trace};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, RcUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirror of SC's `UFont::Serialize` (Engine_demo 0x103ed710):
///   - `Super::Serialize` (UObject, tagged props)
///   - `Ar << Pages << CharactersPerPage`
///   - if `Ar.Ver() >= 69`: `Ar << CharRemap << IsRemapped`
///
/// Notably SC's variant has no `Kerning` and no `LicenseeVer < 0x1D` switch.
/// `Pages` is `TArray<FFontPage>` where `FFontPage = { UTexture* Texture;
/// TArray<FFontCharacter> Characters; }` and `FFontCharacter` is the
/// 4-INT `(StartU, StartV, USize, VSize)`.
#[derive(Default, Debug)]
pub struct Font {
    pub parent_object: Object,

    pub old_pages: Vec<OldFontPage>,
    pub characters_per_page: i32,
    pub char_remap: Vec<(u16, u16)>,
    pub is_remapped: u32,
}

#[derive(Default, Debug)]
pub struct OldFontPage {
    pub texture: Option<RcUnrealObject>,
    pub characters: Vec<OldFontCharacter>,
}

#[derive(Default, Debug)]
pub struct OldFontCharacter {
    pub start_u: i32,
    pub start_v: i32,
    pub u_size: i32,
    pub v_size: i32,
}

impl DeserializeUnrealObject for Font {
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
        let span = span!(Level::DEBUG, "deserialize_font");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let file_ver = linker.borrow().version();

        let page_count = reader.read_packed_int()?;
        assert!(page_count >= 0, "negative page count");
        self.old_pages = (0..page_count)
            .map(|_| -> io::Result<OldFontPage> {
                let texture = reader.read_object::<E>(runtime, linker)?;
                let char_count = reader.read_packed_int()?;
                assert!(char_count >= 0, "negative character count");
                let mut characters = Vec::with_capacity(char_count as usize);
                for _ in 0..char_count {
                    characters.push(OldFontCharacter {
                        start_u: reader.read_i32::<E>()?,
                        start_v: reader.read_i32::<E>()?,
                        u_size: reader.read_i32::<E>()?,
                        v_size: reader.read_i32::<E>()?,
                    });
                }
                Ok(OldFontPage { texture, characters })
            })
            .collect::<io::Result<Vec<_>>>()?;
        self.characters_per_page = reader.read_i32::<E>()?;

        if file_ver >= 69 {
            // TMap<TCHAR,TCHAR>: serialized as TArray<TPair<TCHAR,TCHAR>>.
            let pair_count = reader.read_packed_int()?;
            assert!(pair_count >= 0, "negative CharRemap count");
            self.char_remap = (0..pair_count)
                .map(|_| -> io::Result<(u16, u16)> {
                    Ok((reader.read_u16::<E>()?, reader.read_u16::<E>()?))
                })
                .collect::<io::Result<Vec<_>>>()?;
            self.is_remapped = reader.read_u32::<E>()?;
        }

        trace!(
            "Font: {} pages, chars_per_page={}, {} remap pairs",
            self.old_pages.len(),
            self.characters_per_page,
            self.char_remap.len()
        );

        Ok(())
    }
}
