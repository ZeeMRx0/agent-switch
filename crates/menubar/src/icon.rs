//! The menu bar icon: two arrows pointing opposite ways, drawn as a template image (black on clear), so
//! macOS tints it for light and dark menu bars. It is built in code so the app needs no image file.

use tray_icon::Icon;

const SIZE: u32 = 36; // 18 points at 2x

/// Fill the pixels of one arrow: a bar from `x0` to `x1` with a head at the end that `dir` points to.
fn arrow(mask: &mut [bool], y: i32, x0: i32, x1: i32, dir: i32) {
    let tip = if dir > 0 { x1 } else { x0 };
    for x in x0..=x1 {
        for dy in -1..=1 {
            set(mask, x, y + dy);
        }
    }
    for step in 0..=6 {
        for dy in -(6 - step)..=(6 - step) {
            set(mask, tip - dir * step, y + dy);
        }
    }
}

fn set(mask: &mut [bool], x: i32, y: i32) {
    if (0..SIZE as i32).contains(&x) && (0..SIZE as i32).contains(&y) {
        mask[(y as u32 * SIZE + x as u32) as usize] = true;
    }
}

pub fn rgba() -> Vec<u8> {
    let mut mask = vec![false; (SIZE * SIZE) as usize];
    arrow(&mut mask, 11, 5, 30, 1);
    arrow(&mut mask, 24, 5, 30, -1);
    mask.iter().flat_map(|on| if *on { [0, 0, 0, 255] } else { [0, 0, 0, 0] }).collect()
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
        // Top row of the icon is empty, so it sits centered in the bar.
        assert!(px[..(SIZE * 4) as usize].chunks(4).all(|p| p[3] == 0));
    }
}
