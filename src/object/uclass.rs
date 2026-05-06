use std::io;

use crate::{
    de::RcLinker,
    object::{
        DeserializeUnrealObject, RcUnrealObject, UnrealObject,
        internal::{fdependency::FDependency, fname::FName, property::PropertyTag},
        ustate::State,
    },
    reader::{LinRead, UnrealReadExt},
    runtime::UnrealRuntime,
};
use byteorder::ReadBytesExt;
use tracing::{Level, debug, span, trace};

#[derive(Default, Debug)]
pub struct Class {
    pub parent_object: State,

    pub old_class_record_size: Option<u32>,
    pub class_flags: u32,
    pub class_guid: (u32, u32, u32, u32),
    pub dependencies: Vec<FDependency>,
    pub package_imports: Vec<FName>,
    pub class_within: Option<RcUnrealObject>,
    pub class_config_name: FName,
    pub hide_categories: Vec<FName>,
    pub default_tags: Vec<PropertyTag>,
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

        debug!("parent_object");
        self.parent_object
            .deserialize::<E, _>(runtime, linker, reader)?;

        let version = linker.borrow().version();
        if version <= 61 {
            self.old_class_record_size = Some(reader.read_u32::<E>()?);
        }

        self.class_flags = reader.read_u32::<E>()?;
        self.class_guid = (
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
            reader.read_u32::<E>()?,
        );

        debug!("dependencies");
        self.dependencies = reader.read_serializable_array::<E, FDependency>(runtime, linker)?;

        debug!("package_imports");
        self.package_imports = reader.read_serializable_array::<E, FName>(runtime, linker)?;

        if version >= 62 {
            debug!("class_within");
            self.class_within = reader.read_object::<E>(runtime, linker)?;
            debug!("class_config_name");
            self.class_config_name
                .deserialize::<E, _>(runtime, linker, reader)?;
        }

        if version >= 99 {
            debug!("hide_categories");
            self.hide_categories = reader.read_serializable_array::<E, FName>(runtime, linker)?;
        }

        debug!("default_tags");
        // Reset so the second-pass case (UE2's `Preload` re-runs the serialize
        // body when the inner super-recursion already cleared RF_NeedLoad)
        // doesn't accumulate duplicates.
        self.default_tags.clear();
        loop {
            let mut tag = PropertyTag::default();
            tag.deserialize::<E, _>(runtime, linker, reader)?;
            if tag.name.is_none() {
                break;
            }
            trace!(
                "tag value: type={} size={:#X}",
                tag.property_type, tag.size
            );
            // Match `FPropertyTag::SerializeTaggedProperty` in UnClass.cpp.
            // BoolProperty value is in `Tag.Info & 0x80` (no extra bytes).
            // ObjectProperty / ClassProperty / DelegateProperty values are
            // packed_int object refs that need to trigger `CreateExport`
            // via `read_object`. Other primitives we still consume as raw
            // bytes through `cheat()` since they don't trigger object loads.
            const NAME_BYTE_PROPERTY: u8 = 1;
            const NAME_INT_PROPERTY: u8 = 2;
            const NAME_BOOL_PROPERTY: u8 = 3;
            const NAME_FLOAT_PROPERTY: u8 = 4;
            const NAME_OBJECT_PROPERTY: u8 = 5;
            const NAME_NAME_PROPERTY: u8 = 6;
            const NAME_DELEGATE_PROPERTY: u8 = 7;
            const NAME_CLASS_PROPERTY: u8 = 8;
            match tag.property_type {
                NAME_BOOL_PROPERTY => {
                    // No extra value bytes.
                }
                NAME_OBJECT_PROPERTY | NAME_CLASS_PROPERTY | NAME_DELEGATE_PROPERTY => {
                    let _ = reader.read_object::<E>(runtime, linker)?;
                }
                NAME_NAME_PROPERTY => {
                    let _ = reader.read_packed_int()?;
                }
                NAME_INT_PROPERTY | NAME_FLOAT_PROPERTY => {
                    let mut buf = [0u8; 4];
                    reader.cheat(&mut buf)?;
                }
                NAME_BYTE_PROPERTY => {
                    let mut buf = [0u8; 1];
                    reader.cheat(&mut buf)?;
                }
                _ => {
                    if tag.size > 0 {
                        let mut buf = vec![0u8; tag.size as usize];
                        reader.cheat(&mut buf)?;
                    }
                }
            }
            self.default_tags.push(tag);
        }

        Ok(())
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
