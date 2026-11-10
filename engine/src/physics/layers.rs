//! Collision layers/masks, named layers (RON-configurable) and surface materials.

use serde::{Deserialize, Serialize};

/// A set of up to 32 layers, one bit each.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct LayerMask(pub u32);

impl LayerMask {
    pub const NONE: LayerMask = LayerMask(0);
    pub const ALL: LayerMask = LayerMask(u32::MAX);

    pub const fn bit(index: u32) -> LayerMask {
        LayerMask(1 << index)
    }
    pub const fn contains(self, other: LayerMask) -> bool {
        self.0 & other.0 == other.0
    }
    pub const fn intersects(self, other: LayerMask) -> bool {
        self.0 & other.0 != 0
    }
}

impl std::ops::BitOr for LayerMask {
    type Output = LayerMask;
    fn bitor(self, rhs: LayerMask) -> LayerMask {
        LayerMask(self.0 | rhs.0)
    }
}
impl std::ops::BitAnd for LayerMask {
    type Output = LayerMask;
    fn bitand(self, rhs: LayerMask) -> LayerMask {
        LayerMask(self.0 & rhs.0)
    }
}
impl std::ops::Not for LayerMask {
    type Output = LayerMask;
    fn not(self) -> LayerMask {
        LayerMask(!self.0)
    }
}
impl From<u32> for LayerMask {
    fn from(v: u32) -> Self {
        LayerMask(v)
    }
}

/// Optional per-entity component. Entities without it are on layer 0
/// ("default") and collide with everything, matching the old behaviour.
///
/// Two colliders interact only if **each one's mask includes the other's
/// layer**: `(a.layer & b.mask) != 0 && (b.layer & a.mask) != 0`.
/// `layer` is the set of layers this collider *is on* (usually one bit).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollisionLayers {
    pub layer: LayerMask,
    pub mask: LayerMask,
}

impl Default for CollisionLayers {
    fn default() -> Self {
        Self { layer: LayerMask::bit(0), mask: LayerMask::ALL }
    }
}

impl CollisionLayers {
    pub fn new(layer: LayerMask, mask: LayerMask) -> Self {
        Self { layer, mask }
    }
    pub fn collides_with(&self, other: &CollisionLayers) -> bool {
        self.layer.intersects(other.mask) && other.layer.intersects(self.mask)
    }
}

/// Names for the 32 layer bits, loaded from RON so games name their own
/// layers: `(layers: ["default", "player", "enemy", "projectile", "trigger"])`
/// — the list index is the bit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LayerNames {
    pub layers: Vec<String>,
}

impl Default for LayerNames {
    fn default() -> Self {
        Self { layers: vec!["default".to_string()] }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LayerError {
    #[error("layer config parse error: {0}")]
    Parse(String),
    #[error("more than 32 layers")]
    TooMany,
    #[error("duplicate layer name `{0}`")]
    Duplicate(String),
    #[error("unknown layer `{0}`")]
    Unknown(String),
    #[error("io: {0}")]
    Io(String),
}

impl LayerNames {
    pub fn from_ron(text: &str) -> Result<Self, LayerError> {
        let names: LayerNames = ron::from_str(text).map_err(|e| LayerError::Parse(e.to_string()))?;
        names.validate()?;
        Ok(names)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, LayerError> {
        let text = std::fs::read_to_string(path).map_err(|e| LayerError::Io(e.to_string()))?;
        Self::from_ron(&text)
    }

    fn validate(&self) -> Result<(), LayerError> {
        if self.layers.len() > 32 {
            return Err(LayerError::TooMany);
        }
        for (i, n) in self.layers.iter().enumerate() {
            if self.layers[..i].contains(n) {
                return Err(LayerError::Duplicate(n.clone()));
            }
        }
        Ok(())
    }

    pub fn index(&self, name: &str) -> Option<u32> {
        self.layers.iter().position(|n| n == name).map(|i| i as u32)
    }

    pub fn layer(&self, name: &str) -> Result<LayerMask, LayerError> {
        self.index(name).map(LayerMask::bit).ok_or_else(|| LayerError::Unknown(name.to_string()))
    }

    /// Union of the named layers.
    pub fn mask(&self, names: &[&str]) -> Result<LayerMask, LayerError> {
        names.iter().try_fold(LayerMask::NONE, |acc, n| Ok(acc | self.layer(n)?))
    }

    /// Builds a component: on layer `layer`, colliding with `collides_with`.
    pub fn layers(&self, layer: &str, collides_with: &[&str]) -> Result<CollisionLayers, LayerError> {
        Ok(CollisionLayers { layer: self.layer(layer)?, mask: self.mask(collides_with)? })
    }

    /// Names of the set bits (unnamed bits are shown as `#n`).
    pub fn describe(&self, mask: LayerMask) -> Vec<String> {
        (0..32)
            .filter(|i| mask.0 & (1 << i) != 0)
            .map(|i| self.layers.get(i as usize).cloned().unwrap_or_else(|| format!("#{i}")))
            .collect()
    }
}

/// Optional surface material. Absent = frictionless, non-bouncy (the old
/// behaviour). Pair values combine as `friction = sqrt(a*b)`,
/// `restitution = max(a, b)`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhysicsMaterial {
    pub friction: f32,
    pub restitution: f32,
}

impl Default for PhysicsMaterial {
    fn default() -> Self {
        Self { friction: 0.4, restitution: 0.0 }
    }
}

impl PhysicsMaterial {
    pub const SLIPPERY: PhysicsMaterial = PhysicsMaterial { friction: 0.0, restitution: 0.0 };

    pub fn combine(a: Option<PhysicsMaterial>, b: Option<PhysicsMaterial>) -> PhysicsMaterial {
        let a = a.unwrap_or(Self::SLIPPERY);
        let b = b.unwrap_or(Self::SLIPPERY);
        PhysicsMaterial {
            friction: (a.friction.max(0.0) * b.friction.max(0.0)).sqrt(),
            restitution: a.restitution.max(b.restitution).clamp(0.0, 1.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_algebra() {
        let a = LayerMask::bit(1) | LayerMask::bit(3);
        assert!(a.contains(LayerMask::bit(3)));
        assert!(!a.contains(LayerMask::bit(2)));
        assert!(a.intersects(LayerMask::bit(1) | LayerMask::bit(2)));
        assert_eq!((!a & a), LayerMask::NONE);
    }

    #[test]
    fn collision_rule_is_mutual() {
        let player = CollisionLayers::new(LayerMask::bit(1), LayerMask::bit(0) | LayerMask::bit(2));
        let enemy = CollisionLayers::new(LayerMask::bit(2), LayerMask::ALL);
        let ghost = CollisionLayers::new(LayerMask::bit(3), LayerMask::bit(3));
        assert!(player.collides_with(&enemy) && enemy.collides_with(&player));
        assert!(!player.collides_with(&ghost));
        // One-sided mask: ghost wants player but player doesn't want ghost.
        let ghost2 = CollisionLayers::new(LayerMask::bit(3), LayerMask::ALL);
        assert!(!player.collides_with(&ghost2) && !ghost2.collides_with(&player));
        assert!(CollisionLayers::default().collides_with(&CollisionLayers::default()));
    }

    #[test]
    fn named_layers_from_ron() {
        let names = LayerNames::from_ron(r#"(layers: ["default", "player", "enemy"])"#).unwrap();
        assert_eq!(names.index("enemy"), Some(2));
        assert_eq!(names.mask(&["player", "enemy"]).unwrap(), LayerMask(0b110));
        let l = names.layers("player", &["default", "enemy"]).unwrap();
        assert_eq!(l.layer, LayerMask(0b010));
        assert_eq!(l.mask, LayerMask(0b101));
        assert!(matches!(names.layer("nope"), Err(LayerError::Unknown(_))));
        assert_eq!(names.describe(LayerMask(0b101)), vec!["default", "enemy"]);
    }

    #[test]
    fn layer_config_validation() {
        assert!(matches!(LayerNames::from_ron(r#"(layers: ["a","a"])"#), Err(LayerError::Duplicate(_))));
        let many: Vec<String> = (0..33).map(|i| format!("l{i}")).collect();
        let text = format!("(layers: {:?})", many);
        assert!(matches!(LayerNames::from_ron(&text), Err(LayerError::TooMany)));
        assert!(matches!(LayerNames::from_ron("garbage"), Err(LayerError::Parse(_))));
    }

    #[test]
    fn material_combination() {
        let rubber = PhysicsMaterial { friction: 1.0, restitution: 0.8 };
        let ice = PhysicsMaterial { friction: 0.04, restitution: 0.0 };
        let m = PhysicsMaterial::combine(Some(rubber), Some(ice));
        assert!((m.friction - 0.2).abs() < 1e-5);
        assert_eq!(m.restitution, 0.8);
        assert_eq!(PhysicsMaterial::combine(None, None), PhysicsMaterial::SLIPPERY);
    }
}
