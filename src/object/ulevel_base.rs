use std::io;

use byteorder::ByteOrder;
use byteorder::ReadBytesExt;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::RcUnrealObject;
use crate::object::uobject::Object;
use crate::reader::LinRead;
use crate::reader::UnrealReadExt;
use crate::runtime::UnrealRuntime;

/// Mirrors `ULevelBase::Serialize` (Engine_demo `sub_1040fa10`).
///
/// At normal package load (`IsLoading=1`, `IsTrans=0`, `IsSaving=0`):
/// - Super (UObject tag loop) -- handled via parent_object.deserialize.
/// - `INT DbNum` (raw u32) and `INT DbMax` (raw u32) -- Actors array
///   metadata.
/// - For each of `DbNum` actors: `Ar << UObject*` (packed_int).
/// - `Ar << URL` (FURL operator<<: 4 FStrings + TArray<FString> +
///   2 INTs).
/// - The UT2004 `!IsLoading && !IsSaving` NetDriver/DemoRecDriver
///   tail is **dropped** in SC's body -- confirmed by the
///   `Engine_demo.dll` decompile.
///
/// `IsTrans` branch (calls `sub_104159a0` -> `sub_10415d00`) reads
/// the actors array via the standard `Ar << TArray` path; that's
/// only used in transient archives (Editor undo, etc.), never at
/// package load.
#[derive(Default, Debug)]
pub struct LevelBase {
    pub parent_object: Object,
    pub actors_count: u32,
    pub actors_max: u32,
    pub actors: Vec<Option<RcUnrealObject>>,
    pub url: FUrl,
}

#[derive(Default, Debug)]
pub struct FUrl {
    pub protocol: String,
    pub host: String,
    pub map: String,
    pub portal: String,
    pub op: Vec<String>,
    pub port: u32,
    pub valid: u32,
}

fn read_furl<E, R>(reader: &mut R) -> io::Result<FUrl>
where
    E: ByteOrder,
    R: LinRead,
{
    let protocol = reader.read_string()?;
    let host = reader.read_string()?;
    let map = reader.read_string()?;
    let portal = reader.read_string()?;
    let op_count = reader.read_packed_int()?;
    // Negative TArray counts cannot occur in an engine-written body
    // (the FArray operator<< writes a non-negative ARCount). Reaching
    // a negative value here means the source bytes for this body are
    // misaligned with the engine's read order, almost always because
    // verify_imports cascade-loaded a package whose authoring-time
    // bytes were laid down at a different point in the engine's read
    // sequence. The level body itself is unrecoverable; abort the
    // body load with an io::Error so the surrounding `preload`/
    // `decode_unchecked` path unwinds cleanly and the pair contributes
    // its already-captured siblings via the `run_pair` Err path.
    if op_count < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("FURL op count negative ({op_count}) -- body is misaligned"),
        ));
    }
    let mut op = Vec::with_capacity(op_count as usize);
    for _ in 0..op_count {
        op.push(reader.read_string()?);
    }
    let port = reader.read_u32::<E>()?;
    let valid = reader.read_u32::<E>()?;
    Ok(FUrl {
        protocol,
        host,
        map,
        portal,
        op,
        port,
        valid,
    })
}

impl DeserializeUnrealObject for LevelBase {
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
            .deserialize::<E, _>(runtime, linker, reader)?;

        // Actors array: [DbNum: u32][DbMax: u32][DbNum × packed_int].
        // No CountBytes IO at load time.
        self.actors_count = reader.read_u32::<E>()?;
        self.actors_max = reader.read_u32::<E>()?;
        self.actors = Vec::with_capacity(self.actors_count as usize);
        for _ in 0..self.actors_count {
            self.actors.push(reader.read_object::<E>(runtime, linker)?);
        }

        self.url = read_furl::<E, _>(reader)?;

        Ok(())
    }
}
