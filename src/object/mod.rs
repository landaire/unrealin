#[cfg(test)]
mod test_common;
mod uclass;
mod uobject;
mod ustruct;
use std::io::{self, Read};

use byteorder::ByteOrder;
use paste::paste;
pub mod builtins {
    pub use super::uclass::Class;
    pub use super::uobject::Object;
    pub use super::ustruct::Struct;
}

use builtins::*;

use crate::de::Linker;

pub trait UnrealObject {
    fn name(&self) -> &str;
    fn kind(&self) -> UObjectKind;
    fn parent_object(&self) -> Option<&dyn UnrealObject>;
    fn base_object(&self) -> &Object;
    fn as_any(&self) -> &dyn std::any::Any;
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
    fn is_a(&self, kind: UObjectKind) -> bool;
}

pub trait DeserializeUnrealObject {
    fn deserialize<E, R>(&self, reader: R, linker: &Linker) -> io::Result<()>
    where
        E: ByteOrder,
        R: Read;
}

macro_rules! register_builtins {
    ($($name:ident),*) => {
        #[derive(Debug, Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
        pub enum UObjectKind {
            $(
                $name,
            )*
        }

        impl UObjectKind {
            const fn all() -> &'static [UObjectKind] {
                [
                    $(
                        UObjectKind::$name,
                    )*
                ].as_slice()
            }

            pub fn construct(&self) -> Box<dyn UnrealObject>  {
                match self {
                    $(
                        Self::$name => {
                            Box::new($name::default())
                        }
                    )*
                }
            }

            pub fn as_str(&self) -> &'static str {
                match self {
                    $(
                        Self::$name => {
                            stringify!($name)
                        }
                    )*
                }
            }

            $(
              paste! {
                  pub fn [<is_ $name:lower>](&self) -> bool {
                      matches!(self, UObjectKind::$name)
                  }
              }
            )*
        }

        impl TryFrom<&str> for UObjectKind {
            type Error = ();

            fn try_from(name: &str) -> Result<Self, Self::Error> {
                match name {
                    $(
                        stringify!($name) => {
                            Ok(UObjectKind::$name)
                        }
                    )*
                    _ => {
                        Err(())
                    }
                }
            }
        }
    };
}

register_builtins!(Object, Struct, Class);

macro_rules! make_inherited_object {
    ($($name:ident),*) => {
        $(
            impl UnrealObject for $name {
                fn name(&self) -> &str {
                    self.base_object().name()
                }

                fn kind(&self) -> UObjectKind {
                    UObjectKind::$name
                }

                fn parent_object(&self) -> Option<&dyn UnrealObject> {
                    Some(&self.parent_object)
                }

                fn base_object(&self) -> &Object {
                    let mut current_object = self.parent_object().expect("current_object has no ParentObject");
                    while current_object.kind() != UObjectKind::Object {
                        current_object = current_object.parent_object().expect("current_object has no ParentObject");
                    }

                    current_object.as_any().downcast_ref::<Object>().expect("base object is not an Object")
                }

                fn as_any(&self) -> &dyn std::any::Any {
                    self
                }

                fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
                    self
                }

                fn is_a(&self, kind: UObjectKind) -> bool {
                    let mut current_object = self as &dyn UnrealObject;
                    if current_object.kind() == kind {
                        return true;
                    }

                    while let Some(parent) = current_object.parent_object() {
                        if parent.kind() == kind {
                            return true;
                        }

                        current_object = parent;
                    }

                    false
                }
            }
        )*
    };
}

make_inherited_object!(Struct, Class);
