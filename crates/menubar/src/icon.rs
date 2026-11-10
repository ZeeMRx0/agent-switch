//! The app artwork's hooked arrows, rendered as a black-on-clear macOS template.
//! Regenerate the embedded alpha mask with `sh scripts/generate-icons.sh`.

use tray_icon::Icon;

const SIZE: u32 = 36; // 18 points at 2x
const ALPHA: &[u8; (SIZE * SIZE) as usize] = include_bytes!("../MenuBarIcon.alpha");

pub fn rgba() -> Vec<u8> {
    ALPHA.iter().flat_map(|&alpha| [0, 0, 0, alpha]).collect()
}

pub fn icon() -> Result<Icon, String> {
    Icon::from_rgba(rgba(), SIZE, SIZE).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icon_is_a_valid_template_with_clear_margins() {
        assert!(icon().is_ok());
        let px = rgba();
        assert_eq!(px.len(), (SIZE * SIZE * 4) as usize);
        assert!(px.chunks_exact(4).all(|p| p[..3] == [0, 0, 0]));
        assert!(ALPHA.iter().any(|&a| a == 255));
        for y in 0..SIZE as usize {
            for x in 0..SIZE as usize {
                if x == 0 || y == 0 || x == SIZE as usize - 1 || y == SIZE as usize - 1 {
                    assert_eq!(ALPHA[y * SIZE as usize + x], 0);
                }
            }
        }
    }

    #[test]
    fn the_two_arrows_do_not_touch_even_diagonally() {
        let size = SIZE as usize;
        let mut visited = [false; (SIZE * SIZE) as usize];
        let mut areas = Vec::new();
        for start in 0..ALPHA.len() {
            if visited[start] || ALPHA[start] < 128 {
                continue;
            }
            let mut pending = vec![start];
            visited[start] = true;
            let mut area = 0;
            while let Some(index) = pending.pop() {
                area += 1;
                let (x, y) = (index % size, index / size);
                for ny in y.saturating_sub(1)..=(y + 1).min(size - 1) {
                    for nx in x.saturating_sub(1)..=(x + 1).min(size - 1) {
                        let next = ny * size + nx;
                        if !visited[next] && ALPHA[next] >= 128 {
                            visited[next] = true;
                            pending.push(next);
                        }
                    }
                }
            }
            areas.push(area);
        }
        assert_eq!(
            areas.len(),
            2,
            "the template must contain two separate arrows"
        );
        assert!(
            areas.iter().all(|&area| area > 100),
            "both arrows must remain visible"
        );
    }
}
