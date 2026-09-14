//! The application icon to overwrite the default egui 'e' icon.

/// Icon edge length in pixels. 256 covers every slot macOS and Windows ask for.
const SIZE: usize = 256;

/// A dark rounded square with a sine trace across it, matching the scope.
pub fn icon() -> egui::IconData {
    let mut rgba = vec![0u8; SIZE * SIZE * 4];

    let bg = [15u8, 23, 42];
    let grid = [38u8, 48, 64];
    let trace = [125u8, 211, 252];

    let radius = SIZE as f32 * 0.18;
    let cycles = 1.6_f32;
    let amplitude = SIZE as f32 * 0.22;
    let thickness = SIZE as f32 * 0.045;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let i = (y * SIZE + x) * 4;

            // Rounded-rect mask, so the icon does not read as a hard square
            // next to platform-styled neighbours.
            if outside_rounded_rect(x as f32, y as f32, SIZE as f32, radius) {
                continue; // leave fully transparent
            }

            let mut pixel = bg;

            // Centre grid lines, faint, echoing the scope graticule.
            let mid = SIZE as f32 / 2.0;
            if (x as f32 - mid).abs() < 1.0 || (y as f32 - mid).abs() < 1.0 {
                pixel = grid;
            }

            // The trace.
            let phase = x as f32 / SIZE as f32 * cycles * std::f32::consts::TAU;
            let trace_y = mid - phase.sin() * amplitude;
            if (y as f32 - trace_y).abs() < thickness {
                pixel = trace;
            }

            rgba[i] = pixel[0];
            rgba[i + 1] = pixel[1];
            rgba[i + 2] = pixel[2];
            rgba[i + 3] = 255;
        }
    }

    egui::IconData {
        rgba,
        width: SIZE as u32,
        height: SIZE as u32,
    }
}

/// True when `(x, y)` falls outside a rounded square of side `size`.
fn outside_rounded_rect(x: f32, y: f32, size: f32, radius: f32) -> bool {
    // Distance from the nearest corner circle's centre, only in the corner
    // quadrants; the straight edges are always inside.
    let cx = if x < radius {
        radius
    } else if x > size - radius {
        size - radius
    } else {
        return false;
    };
    let cy = if y < radius {
        radius
    } else if y > size - radius {
        size - radius
    } else {
        return false;
    };
    (x - cx).powi(2) + (y - cy).powi(2) > radius * radius
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_the_declared_size_and_fully_populated() {
        let data = icon();
        assert_eq!(data.width, SIZE as u32);
        assert_eq!(data.height, SIZE as u32);
        assert_eq!(data.rgba.len(), SIZE * SIZE * 4);
    }

    #[test]
    fn corners_are_transparent_and_the_centre_is_not() {
        let data = icon();
        let at = |x: usize, y: usize| data.rgba[(y * SIZE + x) * 4 + 3];
        assert_eq!(at(0, 0), 0, "corner should be rounded away");
        assert_eq!(at(SIZE - 1, SIZE - 1), 0);
        assert_eq!(at(SIZE / 2, SIZE / 2), 255);
    }

    #[test]
    fn the_trace_is_visible_somewhere() {
        let data = icon();
        let trace = [125u8, 211, 252];
        let found = data
            .rgba
            .chunks(4)
            .any(|p| p[0] == trace[0] && p[1] == trace[1] && p[2] == trace[2]);
        assert!(found, "the icon should actually contain a trace");
    }
}
