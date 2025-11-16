use std::{cell::RefCell, io, rc::Rc};

use byteorder::ByteOrder;
use tracing::{Level, debug, event, span, trace};

use crate::{
    de::{Linker, ObjectExport},
    object::{
        DeserializeUnrealObject, NAME_NONE, ObjectFlags, UObjectKind, UnrealObject,
        internal::property::PropertyTag,
    },
    reader::LinRead,
    runtime::UnrealRuntime,
};

#[derive(Debug)]
pub struct Object {
    pub name: String,
    pub flags: ObjectFlags,
    /// The concrete type of this object
    pub concrete_object_kind: Option<UObjectKind>,
    // package_index: usize,
    // class: i32,
    // outer: i32, //RcUnrealObject,
}

impl Object {
    pub fn name(&self) -> &str {
        self.name.as_str()
    }

    pub fn set_name(&mut self, name: String) {
        self.name = name;
    }

    pub fn flags(&self) -> ObjectFlags {
        self.flags
    }

    pub fn set_flags(&mut self, flags: ObjectFlags) {
        self.flags = flags;
    }

    pub fn set_concrete_object_kind(&mut self, kind: UObjectKind) {
        self.concrete_object_kind = Some(kind);
    }

    pub fn concrete_object_kind(&self) -> UObjectKind {
        self.concrete_object_kind.expect("object_kind not set")
    }
}

impl Default for Object {
    fn default() -> Self {
        Self {
            name: "None".to_owned(),
            flags: ObjectFlags::empty(),
            concrete_object_kind: None,
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
        let span = span!(Level::DEBUG, "deserialize_object");
        let _enter = span.enter();

        debug!(
            "Deserializing object with kind {:?}",
            self.concrete_object_kind
        );

        if self.flags.contains(ObjectFlags::HAS_STACK) {
            todo!("UObject HAS_STACK path");
        }

        if self.concrete_object_kind() != UObjectKind::Class {
            let mut properties = Vec::new();
            loop {
                trace!("Deserializing property");
                let mut tag = PropertyTag::default();
                tag.deserialize::<E, _>(runtime, Rc::clone(&linker), reader)?;

                if tag.name as usize == NAME_NONE {
                    break;
                }

                properties.push(tag);
            }
        }

        Ok(())
    }
}

impl UnrealObject for Object {
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
