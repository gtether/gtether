use async_trait::async_trait;
use itertools::{izip, Itertools};
use smol::io::BufReader;
use std::marker::PhantomData;
use std::sync::Arc;
use tobj::futures::load_obj_buf;
use vulkano::pipeline::graphics::vertex_input::Vertex;
use vulkano::Validated;

use crate::render::model::{Model, ModelVertex, ModelVertexNormal, ModelVertexNormalColor, ModelVertexNormalTex};
use crate::render::EngineDevice;
use crate::resource::manager::ResourceManager;
use crate::resource::path::ResourcePath;
use crate::resource::{ResourceLoadError, ResourceLoader, ResourceReadData};

pub struct ModelObjLoader<V: Vertex> {
    device: Arc<EngineDevice>,
    _vertex_type: PhantomData<V>,
}

impl<V: Vertex> ModelObjLoader<V> {
    #[inline]
    pub fn new(device: Arc<EngineDevice>) -> Self {
        Self {
            device,
            _vertex_type: PhantomData,
        }
    }

    async fn load_obj_model(data: ResourceReadData) -> Result<tobj::Model, ResourceLoadError> {
        let mut reader = BufReader::new(data);

        let (mut models, mat_result) = load_obj_buf(
            &mut reader,
            &tobj::GPU_LOAD_OPTIONS,
            move |_| async move {
                // TODO: impl material resource loading
                Err(tobj::LoadError::OpenFileFailed)
            }
        ).await.map_err(ResourceLoadError::from_error)?;

        if models.len() != 1 {
            return Err(ResourceLoadError::from("Only 1 model per file currently supported"))
        }
        let model = models.pop().unwrap();

        let materials = mat_result
            .map_err(ResourceLoadError::from_error)?;
        if !materials.is_empty() {
            return Err(ResourceLoadError::from("Materials are not yet supported"))
        }

        Ok(model)
    }

    fn check_obj_normals(model: &tobj::Model) -> Result<(), ResourceLoadError> {
        if model.mesh.normals.is_empty() {
            Err(ResourceLoadError::from("Normals are missing"))
        } else {
            Ok(())
        }
    }

    fn check_obj_colors(model: &tobj::Model) -> Result<(), ResourceLoadError> {
        if model.mesh.vertex_color.is_empty() {
            Err(ResourceLoadError::from("Vertex colors are missing"))
        } else {
            Ok(())
        }
    }

    fn check_obj_tex_coords(model: &tobj::Model) -> Result<(), ResourceLoadError> {
        if model.mesh.texcoords.is_empty() {
            Err(ResourceLoadError::from("Tex coords are missing"))
        } else {
            Ok(())
        }
    }
    
    fn iter_map_vec2<F: Into<f32>>(iter: impl ExactSizeIterator<Item=F>) -> impl ExactSizeIterator<Item=glm::TVec2<f32>> {
        iter
            .tuples::<(_, _)>()
            .map(|v| glm::vec2(v.0.into(), v.1.into()))
    }
    
    fn iter_map_vec3<F: Into<f32>>(iter: impl ExactSizeIterator<Item=F>) -> impl ExactSizeIterator<Item=glm::TVec3<f32>> {
        iter
            .tuples::<(_, _, _)>()
            .map(|v| glm::vec3(v.0.into(), v.1.into(), v.2.into()))
    }
}

#[async_trait]
impl ResourceLoader<Model<ModelVertex>> for ModelObjLoader<ModelVertex> {
    async fn load(
        &self,
        _manager: &Arc<ResourceManager>,
        _id: ResourcePath,
        data: ResourceReadData,
    ) -> Result<Box<Model<ModelVertex>>, ResourceLoadError> {
        let model = Self::load_obj_model(data).await?;

        let vertices = Self::iter_map_vec3(model.mesh.positions.into_iter())
            .map(|position| ModelVertex {
                position,
            });

        Model::from_raw(&self.device, vertices, Some(model.mesh.indices))
            .map(Box::new)
            .map_err(Validated::unwrap).map_err(ResourceLoadError::from_error)
    }
}

#[async_trait]
impl ResourceLoader<Model<ModelVertexNormal>> for ModelObjLoader<ModelVertexNormal> {
    async fn load(
        &self,
        _manager: &Arc<ResourceManager>,
        _id: ResourcePath,
        data: ResourceReadData,
    ) -> Result<Box<Model<ModelVertexNormal>>, ResourceLoadError> {
        let model = Self::load_obj_model(data).await?;
        Self::check_obj_normals(&model)?;

        let positions = Self::iter_map_vec3(model.mesh.positions.into_iter());
        let normals = Self::iter_map_vec3(model.mesh.normals.into_iter());

        let vertices = izip!(positions, normals)
            .map(|(position, normal)| ModelVertexNormal {
                position,
                normal,
            });

        Model::from_raw(&self.device, vertices, Some(model.mesh.indices))
            .map(Box::new)
            .map_err(Validated::unwrap).map_err(ResourceLoadError::from_error)
    }
}

#[async_trait]
impl ResourceLoader<Model<ModelVertexNormalColor>> for ModelObjLoader<ModelVertexNormalColor> {
    async fn load(
        &self,
        _manager: &Arc<ResourceManager>,
        _id: ResourcePath,
        data: ResourceReadData,
    ) -> Result<Box<Model<ModelVertexNormalColor>>, ResourceLoadError> {
        let model = Self::load_obj_model(data).await?;
        Self::check_obj_normals(&model)?;
        Self::check_obj_colors(&model)?;

        let positions = Self::iter_map_vec3(model.mesh.positions.into_iter());
        let normals = Self::iter_map_vec3(model.mesh.normals.into_iter());
        let colors = Self::iter_map_vec3(model.mesh.vertex_color.into_iter());

        let vertices = izip!(positions, normals, colors)
            .map(|(position, normal, color)| ModelVertexNormalColor {
                position,
                normal,
                color,
            });

        Model::from_raw(&self.device, vertices, Some(model.mesh.indices))
            .map(Box::new)
            .map_err(Validated::unwrap).map_err(ResourceLoadError::from_error)
    }
}

#[async_trait]
impl ResourceLoader<Model<ModelVertexNormalTex>> for ModelObjLoader<ModelVertexNormalTex> {
    async fn load(
        &self,
        _manager: &Arc<ResourceManager>,
        _id: ResourcePath,
        data: ResourceReadData,
    ) -> Result<Box<Model<ModelVertexNormalTex>>, ResourceLoadError> {
        let model = Self::load_obj_model(data).await?;
        Self::check_obj_normals(&model)?;
        Self::check_obj_tex_coords(&model)?;

        let positions = Self::iter_map_vec3(model.mesh.positions.into_iter());
        let normals = Self::iter_map_vec3(model.mesh.normals.into_iter());
        let tex_coords = Self::iter_map_vec2(model.mesh.texcoords.into_iter());

        let vertices = izip!(positions, normals, tex_coords)
            .map(|(position, normal, tex_coord)| ModelVertexNormalTex {
                position,
                normal,
                tex_coord,
            });

        Model::from_raw(&self.device, vertices, Some(model.mesh.indices))
            .map(Box::new)
            .map_err(Validated::unwrap).map_err(ResourceLoadError::from_error)
    }
}