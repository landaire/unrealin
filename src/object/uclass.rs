use std::{
    cell::RefCell,
    io::{self, SeekFrom},
    rc::Rc,
};

use crate::{
    de::{Linker, ObjectExport},
    object::{DeserializeUnrealObject, UnrealObject, state::State, ustruct::Struct},
    reader::LinRead,
    runtime::UnrealRuntime,
};
use byteorder::ReadBytesExt;

#[derive(Default, Debug)]
pub struct Class {
    pub parent_object: State,
}

impl DeserializeUnrealObject for Class {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        reader.read_u32::<E>()?;
        todo!("class deserialization")
    }
}

#[cfg(test)]
mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    #[test]
    fn test_is_a() {
        let expected_kinds = [
            UObjectKind::Object,
            UObjectKind::Struct,
            UObjectKind::Class,
            UObjectKind::Field,
            UObjectKind::State,
        ];
        let test_obj = Class::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
