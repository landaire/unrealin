use std::{cell::RefCell, io, rc::Rc};

use byteorder::ReadBytesExt;

use crate::{
    de::Linker,
    object::{DeserializeUnrealObject, uobject::Object},
    reader::LinRead,
    runtime::UnrealRuntime,
};

#[derive(Default, Debug)]
pub struct TextBuffer {
    pub parent_object: Object,
}

impl DeserializeUnrealObject for TextBuffer {
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
        panic!("TEXT BUFFER");
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
        let expected_kinds = [UObjectKind::Object, UObjectKind::TextBuffer];
        let test_obj = TextBuffer::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
