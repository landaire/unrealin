use std::io;

use byteorder::{ByteOrder, ReadBytesExt};
use serde::{Deserialize, Serialize};

use crate::de::RcLinker;
use crate::object::{DeserializeUnrealObject, uobject::Object};
use crate::reader::LinRead;
use crate::runtime::UnrealRuntime;

#[derive(Debug, Default)]
pub struct Primitive {
    pub parent_object: Object,
    pub bounding_box: BoundingBox,
    pub bounding_sphere: BoundingSphere,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BoundingBox {
    pub min: FVector,
    pub max: FVector,
    pub is_valid: u8,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct BoundingSphere {
    pub center: FVector,
    pub radius: f32,
}

#[derive(Debug, Default, Deserialize, Serialize)]
pub struct FVector {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl DeserializeUnrealObject for Primitive {
    fn deserialize<E, R>(
        &mut self,
        runtime: &mut UnrealRuntime,
        linker: &RcLinker,
        reader: &mut R,
    ) -> io::Result<()>
    where
        E: ByteOrder,
        R: LinRead,
    {
        self.parent_object.deserialize::<E, R>(runtime, linker, reader)?;
        
        // Read BoundingBox
        self.bounding_box.min.x = reader.read_f32::<E>()?;
        self.bounding_box.min.y = reader.read_f32::<E>()?;
        self.bounding_box.min.z = reader.read_f32::<E>()?;
        self.bounding_box.max.x = reader.read_f32::<E>()?;
        self.bounding_box.max.y = reader.read_f32::<E>()?;
        self.bounding_box.max.z = reader.read_f32::<E>()?;
        self.bounding_box.is_valid = reader.read_u8()?;
        
        // Read BoundingSphere
        self.bounding_sphere.center.x = reader.read_f32::<E>()?;
        self.bounding_sphere.center.y = reader.read_f32::<E>()?;
        self.bounding_sphere.center.z = reader.read_f32::<E>()?;
        self.bounding_sphere.radius = reader.read_f32::<E>()?;
        
        Ok(())
    }
}