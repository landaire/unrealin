use std::{cell::RefCell, io, rc::Rc};

use byteorder::ByteOrder;

use crate::{
    de::{Linker, ObjectExport},
    object::{
        DeserializeUnrealObject, NAME_NONE, ObjectFlags, UObjectKind, UnrealObject,
        internal::property::PropertyTag,
    },
    reader::LinRead,
    runtime::UnrealRuntime,
};

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Object {
    pub name: String,
    pub flags: ObjectFlags,
    // package_index: usize,
    // class: i32,
    // outer: i32, //RcUnrealObject,
}

impl Default for Object {
    fn default() -> Self {
        Self {
            name: "None".to_owned(),
            flags: ObjectFlags::empty(),
        }
    }
}

impl DeserializeUnrealObject for Object {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: Rc<RefCell<Linker>>,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        if self.flags.contains(ObjectFlags::HAS_STACK) {
            todo!("UObject HAS_STACK path");
        }

        let mut properties = Vec::new();
        loop {
            let mut tag = PropertyTag::default();
            tag.deserialize::<E, _>(runtime, Rc::clone(&linker), reader)?;

            if tag.name as usize == NAME_NONE {
                break;
            }

            properties.push(tag);
        }

        Ok(())
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

    fn set_name(&mut self, name: String) {
        self.name = name;
    }

    fn flags(&self) -> ObjectFlags {
        self.flags
    }

    fn set_flags(&mut self, flags: ObjectFlags) {
        self.flags = flags;
    }

    fn parent_object_mut(&mut self) -> Option<&mut dyn UnrealObject> {
        None
    }

    fn base_object_mut(&mut self) -> &mut Object {
        self
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
