//! The menu bar icon: an S-shaped ribbon with an arrowhead at each end, like the app icon. It is drawn
//! as a template image (black on clear), so macOS tints it for light and dark menu bars. It is built in
//! code so the app needs no image file.

use std::f32::consts::FRAC_PI_2;

use tray_icon::Icon;

const SIZE: u32 = 36; // 18 points at 2x
const STROKE: f32 = 3.2; // ribbon width in pixels
const SAMPLES: u32 = 4; // per axis, for smooth edges

/// The ribbon's centre line, from the top arrow's base to the bottom arrow's base.
fn path() -> Vec<(f32, f32)> {
    let mut pts = vec![(25.0, 9.0)];
    // Top bend: from the top of a circle round its left side to its bottom.
    arc(&mut pts, (16.0, 13.5), 4.5, -FRAC_PI_2, -3.0 * FRAC_PI_2);
    // Bottom bend: from the top of a circle round its right side to its bottom.
    arc(&mut pts, (20.0, 22.5), 4.5, -FRAC_PI_2, FRAC_PI_2);
    pts.push((11.0, 27.0));
    pts
}

fn arc(pts: &mut Vec<(f32, f32)>, (cx, cy): (f32, f32), r: f32, from: f32, to: f32) {
    const STEPS: u32 = 24;
    for i in 0..=STEPS {
        let a = from + (to - from) * i as f32 / STEPS as f32;
        pts.push((cx + r * a.cos(), cy + r * a.sin()));
    }
}

/// The two arrowheads, as triangles: one pointing right at the top, one pointing left at the bottom.
const HEADS: [[(f32, f32); 3]; 2] = [
    [(33.0, 9.0), (25.0, 3.0), (25.0, 15.0)],
    [(3.0, 27.0), (11.0, 21.0), (11.0, 33.0)],
];

fn dist_to_segment((px, py): (f32, f32), (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
}

fn in_triangle(p: (f32, f32), [a, b, c]: &[(f32, f32); 3]) -> bool {
    let side = |u: (f32, f32), v: (f32, f32)| (v.0 - u.0) * (p.1 - u.1) - (v.1 - u.1) * (p.0 - u.0);
    let (s1, s2, s3) = (side(*a, *b), side(*b, *c), side(*c, *a));
    (s1 >= 0.0 && s2 >= 0.0 && s3 >= 0.0) || (s1 <= 0.0 && s2 <= 0.0 && s3 <= 0.0)
}

fn covered(p: (f32, f32), path: &[(f32, f32)]) -> bool {
    HEADS.iter().any(|t| in_triangle(p, t))
        || path.windows(2).any(|s| dist_to_segment(p, s[0], s[1]) <= STROKE / 2.0)
}

pub fn rgba() -> Vec<u8> {
    let path = path();
    let mut px = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for y in 0..SIZE {
        for x in 0..SIZE {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let p = (
                        x as f32 + (sx as f32 + 0.5) / SAMPLES as f32,
                        y as f32 + (sy as f32 + 0.5) / SAMPLES as f32,
                    );
                    hits += covered(p, &path) as u32;
                }
            }
            let alpha = (hits * 255 / (SAMPLES * SAMPLES)) as u8;
            px.extend_from_slice(&[0, 0, 0, alpha]);
        }
    }
    px
}

pub fn icon() -> Result<Icon, String> {
    Icon::from_rgba(rgba(), SIZE, SIZE).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_valid_template_with_both_arrows() {
        assert!(icon().is_ok());
        let px = rgba();
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        let opaque = px.chunks(4).filter(|p| p[3] == 255).count();
        assert!((100..SIZE as usize * SIZE as usize / 2).contains(&opaque), "{opaque}");
        // Template image: every pixel is black, only the alpha varies.
        assert!(px.chunks(4).all(|p| p[..3] == [0, 0, 0]));
        // Top row of the icon is empty, so it sits centered in the bar.
        assert!(px[..(SIZE * 4) as usize].chunks(4).all(|p| p[3] == 0));
    }

    #[test]
    fn the_arrowheads_point_right_at_the_top_and_left_at_the_bottom() {
        let px = rgba();
        let alpha = |x: u32, y: u32| px[((y * SIZE + x) * 4 + 3) as usize];
        assert_eq!(alpha(29, 9), 255); // near the top tip, on the right
        assert_eq!(alpha(4, 9), 0);
        assert_eq!(alpha(6, 27), 255); // near the bottom tip, on the left
        assert_eq!(alpha(31, 27), 0);
    }
}
