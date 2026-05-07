use std::io;

use byteorder::{ByteOrder, ReadBytesExt};

use crate::{
    de::RcLinker,
    object::{DeserializeUnrealObject, RcUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

/// Mirrors `ULevelBase::Serialize` (Engine_demo `sub_1040fa10`).
///
/// At normal package load (`IsLoading=1`, `IsTrans=0`, `IsSaving=0`):
/// - Super (UObject tag loop) — handled via parent_object.deserialize.
/// - `INT DbNum` (raw u32) and `INT DbMax` (raw u32) — Actors array
///   metadata.
/// - For each of `DbNum` actors: `Ar << UObject*` (packed_int).
/// - `Ar << URL` (FURL operator<<: 4 FStrings + TArray<FString> +
///   2 INTs).
/// - The UT2004 `!IsLoading && !IsSaving` NetDriver/DemoRecDriver
///   tail is **dropped** in SC's body — confirmed by the
///   `Engine_demo.dll` decompile.
///
/// `IsTrans` branch reads the actors array via the standard
/// `Ar << TArray` path; that's only used in transient archives
/// (Editor undo, etc.), never at package load.
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
    assert!(op_count >= 0, "FURL op count negative");
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
