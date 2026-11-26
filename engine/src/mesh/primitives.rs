use glam::Vec3;

use super::{MeshData, Vertex};

/// Appends one quad (as two triangles) with a per-corner UV and a single
/// flat normal — matches the hand-authored cube OBJ's per-face convention.
fn face(vertices: &mut Vec<Vertex>, indices: &mut Vec<u32>, corners: [Vec3; 4], normal: Vec3) {
    let base = vertices.len() as u32;
    let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
    for (corner, uv) in corners.into_iter().zip(uvs) {
        vertices.push(Vertex {
            position: corner.to_array(),
            normal: normal.to_array(),
            uv,
            color: [0.0, 0.0, 0.0],
        });
    }
    indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
}

/// A unit cube (-0.5..=0.5 on each axis) — scale it via `Transform::scale`.
pub fn cube() -> MeshData {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    let v1 = Vec3::new(-0.5, -0.5, 0.5);
    let v2 = Vec3::new(0.5, -0.5, 0.5);
    let v3 = Vec3::new(0.5, 0.5, 0.5);
    let v4 = Vec3::new(-0.5, 0.5, 0.5);
    let v5 = Vec3::new(-0.5, -0.5, -0.5);
    let v6 = Vec3::new(0.5, -0.5, -0.5);
    let v7 = Vec3::new(0.5, 0.5, -0.5);
    let v8 = Vec3::new(-0.5, 0.5, -0.5);

    face(&mut vertices, &mut indices, [v1, v2, v3, v4], Vec3::Z);
    face(&mut vertices, &mut indices, [v6, v5, v8, v7], Vec3::NEG_Z);
    face(&mut vertices, &mut indices, [v2, v6, v7, v3], Vec3::X);
    face(&mut vertices, &mut indices, [v5, v1, v4, v8], Vec3::NEG_X);
    face(&mut vertices, &mut indices, [v4, v3, v7, v8], Vec3::Y);
    face(&mut vertices, &mut indices, [v5, v6, v2, v1], Vec3::NEG_Y);

    MeshData { vertices, indices }
}

/// A 2x2 unit quad in the XZ plane facing +Y — a simple floor/ground piece.
pub fn plane() -> MeshData {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    let p1 = Vec3::new(-1.0, 0.0, 1.0);
    let p2 = Vec3::new(1.0, 0.0, 1.0);
    let p3 = Vec3::new(1.0, 0.0, -1.0);
    let p4 = Vec3::new(-1.0, 0.0, -1.0);
    face(&mut vertices, &mut indices, [p1, p2, p3, p4], Vec3::Y);

    MeshData { vertices, indices }
}

/// A 1x1 unit quad in the XY plane facing +Z — `dust_engine::particles`'
/// billboard shape (scaled/rotated-to-face-camera per particle at draw
/// time, not baked into the mesh itself).
pub fn quad() -> MeshData {
    let mut vertices = Vec::new();
    let mut indices = Vec::new();

    let p1 = Vec3::new(-0.5, -0.5, 0.0);
    let p2 = Vec3::new(0.5, -0.5, 0.0);
    let p3 = Vec3::new(0.5, 0.5, 0.0);
    let p4 = Vec3::new(-0.5, 0.5, 0.0);
    face(&mut vertices, &mut indices, [p1, p2, p3, p4], Vec3::Z);

    MeshData { vertices, indices }
}

/// A UV sphere of radius 0.5 with smooth normals. `segments` (around) is
/// clamped to at least 3 and `rings` (pole to pole) to at least 2.
pub fn uv_sphere(segments: u32, rings: u32) -> MeshData {
    let segments = segments.max(3);
    let rings = rings.max(2);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for r in 0..=rings {
        let v = r as f32 / rings as f32;
        let phi = v * std::f32::consts::PI;
        for s in 0..=segments {
            let u = s as f32 / segments as f32;
            let theta = u * std::f32::consts::TAU;
            let n = Vec3::new(phi.sin() * theta.cos(), phi.cos(), phi.sin() * theta.sin());
            vertices.push(Vertex {
                position: (n * 0.5).to_array(),
                normal: n.to_array(),
                uv: [u, v],
                color: [0.0; 3],
            });
        }
    }
    let row = segments + 1;
    for r in 0..rings {
        for s in 0..segments {
            let a = r * row + s;
            let b = a + row;
            indices.extend_from_slice(&[a, a + 1, b, a + 1, b + 1, b]);
        }
    }
    MeshData { vertices, indices }
}

/// A capped cylinder of radius 0.5 from y = -0.5 to 0.5 with smooth sides
/// and flat caps. `segments` is clamped to at least 3.
pub fn cylinder(segments: u32) -> MeshData {
    let segments = segments.max(3);
    let mut vertices = Vec::new();
    let mut indices = Vec::new();
    for s in 0..=segments {
        let u = s as f32 / segments as f32;
        let (sin, cos) = (u * std::f32::consts::TAU).sin_cos();
        let n = Vec3::new(cos, 0.0, sin);
        for (y, v) in [(-0.5, 1.0), (0.5, 0.0)] {
            vertices.push(Vertex {
                position: [cos * 0.5, y, sin * 0.5],
                normal: n.to_array(),
                uv: [u, v],
                color: [0.0; 3],
            });
        }
    }
    for s in 0..segments {
        let a = s * 2;
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 1, a + 3, a + 2]);
    }
    for (y, ny) in [(0.5f32, 1.0f32), (-0.5, -1.0)] {
        let centre = vertices.len() as u32;
        vertices.push(Vertex {
            position: [0.0, y, 0.0],
            normal: [0.0, ny, 0.0],
            uv: [0.5, 0.5],
            color: [0.0; 3],
        });
        for s in 0..=segments {
            let (sin, cos) = (s as f32 / segments as f32 * std::f32::consts::TAU).sin_cos();
            vertices.push(Vertex {
                position: [cos * 0.5, y, sin * 0.5],
                normal: [0.0, ny, 0.0],
                uv: [0.5 + cos * 0.5, 0.5 + sin * 0.5],
                color: [0.0; 3],
            });
        }
        for s in 0..segments {
            let (a, b) = (centre + 1 + s, centre + 2 + s);
            if ny > 0.0 {
                indices.extend_from_slice(&[centre, b, a]);
            } else {
                indices.extend_from_slice(&[centre, a, b]);
            }
        }
    }
    MeshData { vertices, indices }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(mesh: &MeshData) {
        assert!(mesh.indices.iter().all(|&i| (i as usize) < mesh.vertices.len()));
        assert_eq!(mesh.indices.len() % 3, 0);
        for v in &mesh.vertices {
            let n = Vec3::from(v.normal);
            assert!((n.length() - 1.0).abs() < 1e-4);
        }
    }

    #[test]
    fn sphere_is_well_formed_and_on_its_radius() {
        let m = uv_sphere(16, 8);
        check(&m);
        assert_eq!(m.indices.len(), 16 * 8 * 6);
        assert!(m.vertices.iter().all(|v| (Vec3::from(v.position).length() - 0.5).abs() < 1e-4));
    }

    #[test]
    fn cylinder_is_well_formed_and_caps_face_outward() {
        let m = cylinder(12);
        check(&m);
        // Every cap triangle's winding agrees with its normal (CCW from outside).
        for tri in m.indices.chunks(3) {
            let [a, b, c] = [0, 1, 2].map(|k| Vec3::from(m.vertices[tri[k] as usize].position));
            let n = Vec3::from(m.vertices[tri[0] as usize].normal);
            if n.y.abs() > 0.5 {
                assert!((b - a).cross(c - a).dot(n) > 0.0);
            }
        }
    }
}
