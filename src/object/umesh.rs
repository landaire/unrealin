use std::io;

use byteorder::ByteOrder;

use crate::de::RcLinker;
use crate::object::DeserializeUnrealObject;
use crate::object::uprimitive::Primitive;
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

/// Mirrors `UMesh::Serialize`. UT2004 source (UnMesh.cpp:20):
/// ```cpp
/// Super::Serialize(Ar);
/// if (!Ar.IsPersistent()) Ar << DefMeshInstance;
/// ```
/// At normal package load `Ar.IsPersistent()` is true, so the
/// `DefMeshInstance` ref is NOT serialized. SC's `sub_1041df50`
/// has the same shape (the conditional gates on what the
/// `Engine_demo` BinAssist subagent called `Ar[7]` -- empirically
/// equivalent to `IsPersistent`, not `GIsSavegame`). Net effect at
/// load time: `UMesh::Serialize` is just `UPrimitive::Serialize`.
#[derive(Default, Debug)]
pub struct Mesh {
    pub parent_object: Primitive,
}

impl DeserializeUnrealObject for Mesh {
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
