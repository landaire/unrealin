use std::{cell::RefCell, collections::HashMap, rc::Rc};

use winnow::BStr;

use crate::{
    common::normalize_index,
    de::{Import, ObjectExport, UnrealPackage},
};

pub type RcUnrealObject = Rc<RefCell<UnrealObject>>;

#[derive(Debug)]
pub(crate) struct UnrealObject {
    name: String,
    package_index: usize,
    class: i32,
    outer: i32, //RcUnrealObject,
}

pub(crate) fn create_import_object<'i>(export: &Import, input: &mut &'i [u8]) -> RcUnrealObject {
    todo!("")
}

pub(crate) fn create_export_object<'i>(
    index: i32,
    export: &ObjectExport<'i>,
    input: &mut &'i [u8],
) -> RcUnrealObject {
    if export.package_index == 0 {
        assert!(export.object_flags & 1 == 0);
    }

    let object = UnrealObject {
        name: "Test".to_string(),
        package_index: normalize_index(index),
        class: export.class_index,
        outer: export.super_index,
    };

    Rc::new(RefCell::new(object))
}

// impl<'i> UnrealPackage<'i> {
//     pub(crate) fn index_to_object(&mut self, index: i32, input: &mut &'i [u8]) -> RcUnrealObject {
//         if index < 0 {
//             let import = &mut self.raw_package.imports[normalize_index(index)];
//             if let Some(obj) = import.object.as_ref() {
//                 return Rc::clone(obj);
//             } else {
//                 let obj = create_import_object(import, input);
//                 import.object = Some(Rc::clone(&obj));

//                 return obj;
//             }
//         }

//         if index > 0 {
//             let export = &mut self.raw_package.exports[normalize_index(index)];

//             if let Some(obj) = export.object.as_ref() {
//                 return Rc::clone(obj);
//             } else {
//                 let obj = create_export_object(index, export, input);
//                 export.object = Some(Rc::clone(&obj));

//                 return obj;
//             }
//         }

//         panic!("unhandled non-existent index");
//     }
// }
