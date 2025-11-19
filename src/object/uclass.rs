use std::{
    cell::RefCell,
    io::{self, SeekFrom},
    rc::Rc,
};

use crate::{
    de::{Linker, ObjectExport, RcLinker},
    object::{
        DeserializeUnrealObject, UnrealObject, builtins::Link, ustate::State, ustruct::Struct,
    },
    reader::LinRead,
    runtime::UnrealRuntime,
};
use byteorder::ReadBytesExt;
use tracing::{Level, span};

#[derive(Default, Debug)]
pub struct Class {
    pub parent_object: State,
}

impl DeserializeUnrealObject for Class {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        let span = span!(Level::DEBUG, "deserialize_class");
        let _enter = span.enter();

        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        reader.read_u32::<E>()?;
        todo!("class deserialization")
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    pub fn expected_uobjectkind() -> impl IntoIterator<Item = UObjectKind> {
        [UObjectKind::Class]
            .iter()
            .cloned()
            .chain(crate::object::ustate::tests::expected_uobjectkind())
    }

    #[test]
    fn test_is_a() {
        let test_obj = Class::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_uobjectkind());
    }
}
