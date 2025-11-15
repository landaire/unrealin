use crate::object::{DeserializeUnrealObject, UnrealObject, ustruct::Struct};

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Class {
    pub parent_object: Struct,
}

impl DeserializeUnrealObject for Class {
    fn deserialize<E, R>(&self, reader: R, linker: &crate::de::Linker) -> std::io::Result<()>
    where
        E: byteorder::ByteOrder,
        R: std::io::Read,
    {
        todo!("class deserialization")
    }
}

#[cfg(test)]
mod tests {
    use byteorder::LittleEndian;

    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    #[test]
    fn test_is_a() {
        let expected_kinds = [UObjectKind::Object, UObjectKind::Struct, UObjectKind::Class];
        let test_obj = Class::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
