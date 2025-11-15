use byteorder::ByteOrder;

use crate::object::{DeserializeUnrealObject, UObjectKind, UnrealObject};

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Object {
    pub name: String,
    // package_index: usize,
    // class: i32,
    // outer: i32, //RcUnrealObject,
}

impl DeserializeUnrealObject for Object {
    fn deserialize<E, R>(&self, reader: R, linker: &crate::de::Linker) -> std::io::Result<()>
    where
        E: ByteOrder,
        R: std::io::Read,
    {
        todo!()
    }
}

impl UnrealObject for Object {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> UObjectKind {
        UObjectKind::Object
    }

    fn parent_object(&self) -> Option<&dyn UnrealObject> {
        None
    }

    fn base_object(&self) -> &Object {
        self
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn is_a(&self, kind: UObjectKind) -> bool {
        self.kind() == kind
    }
}

#[cfg(test)]
mod tests {
    use byteorder::LittleEndian;

    use crate::object::{UnrealObject, test_common::test_object_is_a};

    use super::*;

    #[test]
    fn test_is_a() {
        let expected_kinds = [UObjectKind::Object];
        let test_obj = Object::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
