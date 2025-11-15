use crate::object::uobject::Object;

#[derive(Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Struct {
    pub parent_object: Object,
}

#[cfg(test)]
mod tests {
    use byteorder::LittleEndian;

    use crate::object::{UObjectKind, UnrealObject, test_common::test_object_is_a};

    use super::*;

    #[test]
    fn test_is_a() {
        let expected_kinds = [UObjectKind::Object, UObjectKind::Struct];
        let test_obj = Struct::default();

        test_object_is_a(&test_obj as &dyn UnrealObject, expected_kinds.as_slice());
    }
}
