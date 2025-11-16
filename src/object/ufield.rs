use std::{
    cell::RefCell,
    io::{Read, Seek},
    rc::Rc,
};

use byteorder::ReadBytesExt;

use crate::{
    de::Linker,
    object::{DeserializeUnrealObject, uobject::Object},
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Field {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for Field {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: LinRead,
    {
        self.parent_object
            .deserialize::<E, _>(runtime, Rc::clone(&linker), reader)?;

        let super_field = reader.read_object::<E>(runtime, Rc::clone(&linker))?;
        panic!("{:#?}", super_field);
        let next = reader.read_object::<E>(runtime, Rc::clone(&linker))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    #[test]
    fn test_is_a() {
        let expected_kinds = [UObjectKind::Object, UObjectKind::Field];
        let test_obj = Field::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
