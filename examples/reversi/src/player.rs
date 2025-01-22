#[derive(Debug, Clone)]
pub struct Player {
    name: String,
    color: glm::TVec3<f32>,
}

impl PartialEq for Player {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Player {}

impl Player {
    #[inline]
    pub fn new(
        name: impl Into<String>,
        color: glm::TVec3<f32>,
    ) -> Self {
        Self {
            name: name.into(),
            color,
        }
    }

    #[inline]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[inline]
    pub fn color(&self) -> glm::TVec3<f32> {
        self.color
    }
}