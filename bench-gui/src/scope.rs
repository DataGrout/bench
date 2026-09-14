//! The scope trace — drawn directly from the ring, every frame, no DG.
//!
//! This is the half of Bench that must never touch the gateway.
//! Everything here is a painter call over a slice of `f32`.

use egui::{Color32, Pos2, Rect, Sense, Stroke, Ui, Vec2};

/// Display settings for the trace.
pub struct ScopeStyle {
    pub height: f32,
    pub trace: Color32,
    pub grid: Color32,
    /// Vertical range. `None` auto-scales to the frame.
    pub y_range: Option<(f32, f32)>,
    pub divisions: usize,
    /// Shade `(start, len)` of the samples — the gate the analysis frame
    /// covers — so the trace and the step outputs share a visible time axis.
    pub gate: Option<(usize, usize)>,
}

/// What [`draw_scope`] drew, so the caller can turn pointer input into pans
/// and gate moves in the same sample coordinates the trace used.
pub struct ScopeOutput {
    pub lo: f32,
    pub hi: f32,
    pub rect: Rect,
    /// The gate's on-screen band, if one was drawn.
    pub gate_rect: Option<Rect>,
    pub response: egui::Response,
}

impl Default for ScopeStyle {
    fn default() -> Self {
        Self {
            height: 220.0,
            trace: Color32::from_rgb(125, 211, 252),
            grid: Color32::from_rgb(38, 48, 64),
            y_range: None,
            divisions: 8,
            gate: None,
        }
    }
}

/// Draw a trace that can be dragged: the response senses click-and-drag and
/// the returned geometry lets the caller map pixels back to samples. The
/// vertical range actually used comes back too, so a caller can label the
/// axis with the same numbers that were drawn.
pub fn draw_scope(ui: &mut Ui, samples: &[f32], style: &ScopeStyle) -> ScopeOutput {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), style.height),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);

    painter.rect_filled(rect, 4.0_f32, Color32::from_rgb(15, 23, 42));
    draw_grid(&painter, rect, style);

    let mut gate_rect = None;
    if let Some((start, len)) = style.gate.filter(|(_, l)| *l > 0 && !samples.is_empty()) {
        let n = samples.len() as f32;
        let x0 = rect.left() + rect.width() * (start.min(samples.len()) as f32 / n);
        let x1 = rect.left() + rect.width() * ((start + len).min(samples.len()) as f32 / n);
        let band = Rect::from_min_max(Pos2::new(x0, rect.top()), Pos2::new(x1, rect.bottom()));
        let edge = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(125, 211, 252, 110));
        painter.rect_filled(
            band,
            0.0_f32,
            Color32::from_rgba_unmultiplied(125, 211, 252, 16),
        );
        painter.line_segment([band.left_top(), band.left_bottom()], edge);
        painter.line_segment([band.right_top(), band.right_bottom()], edge);
        painter.text(
            Pos2::new(x0 + 4.0, rect.top() + 3.0),
            egui::Align2::LEFT_TOP,
            "analysis frame",
            egui::FontId::proportional(10.0),
            Color32::from_rgba_unmultiplied(125, 211, 252, 150),
        );
        gate_rect = Some(band);
    }

    if samples.is_empty() {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "no signal",
            egui::FontId::proportional(13.0),
            Color32::from_rgb(100, 116, 139),
        );
        return ScopeOutput {
            lo: -1.0,
            hi: 1.0,
            rect,
            gate_rect,
            response,
        };
    }

    let (min, max) = style.y_range.unwrap_or_else(|| auto_range(samples));
    let range = if (max - min).abs() < 1e-9 {
        1.0
    } else {
        max - min
    };

    // One point per horizontal pixel at most: a 131k-sample ring across an
    // 800px viewport is 160 samples per pixel, and drawing all of them is
    // invisible work. Min/max decimation keeps transients that plain
    // subsampling would drop — a spike between two sampled indices is exactly
    // what a scope exists to show.
    let width = rect.width().max(1.0) as usize;
    let points = decimate_min_max(samples, width);

    let dx = if points.len() > 1 {
        rect.width() / (points.len() - 1) as f32
    } else {
        rect.width()
    };

    let line: Vec<Pos2> = points
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let t = ((v - min) / range).clamp(0.0, 1.0);
            Pos2::new(
                rect.left() + i as f32 * dx,
                rect.bottom() - t * rect.height(),
            )
        })
        .collect();

    painter.add(egui::Shape::line(line, Stroke::new(1.2_f32, style.trace)));
    ScopeOutput {
        lo: min,
        hi: max,
        rect,
        gate_rect,
        response,
    }
}

/// The whole ring as a thin strip, with the current view and the gate marked.
///
/// `view` and `gate` are `(start, end)` fractions of the strip, where 0 is the
/// oldest sample held and 1 is live. Senses click-and-drag so the caller can
/// turn a pointer position into a view position.
pub fn draw_minimap(
    ui: &mut Ui,
    samples: &[f32],
    view: (f32, f32),
    gate: (f32, f32),
) -> (Rect, egui::Response) {
    let (rect, response) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 28.0),
        Sense::click_and_drag(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 3.0_f32, Color32::from_rgb(10, 16, 30));

    if !samples.is_empty() {
        let (min, max) = auto_range(samples);
        let range = (max - min).max(1e-9);
        let points = decimate_min_max(samples, rect.width().max(1.0) as usize);
        let dx = rect.width() / (points.len().max(2) - 1) as f32;
        let line: Vec<Pos2> = points
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let t = ((v - min) / range).clamp(0.0, 1.0);
                Pos2::new(
                    rect.left() + i as f32 * dx,
                    rect.bottom() - 2.0 - t * (rect.height() - 4.0),
                )
            })
            .collect();
        painter.add(egui::Shape::line(
            line,
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(125, 211, 252, 110)),
        ));
    }

    let span = |(a, b): (f32, f32)| {
        Rect::from_min_max(
            Pos2::new(rect.left() + rect.width() * a.clamp(0.0, 1.0), rect.top()),
            Pos2::new(
                rect.left() + rect.width() * b.clamp(0.0, 1.0),
                rect.bottom(),
            ),
        )
    };
    let view_rect = span(view);
    painter.rect_filled(
        view_rect,
        0.0_f32,
        Color32::from_rgba_unmultiplied(226, 232, 240, 26),
    );
    painter.rect_stroke(
        view_rect,
        0.0_f32,
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(226, 232, 240, 150)),
    );
    painter.rect_filled(
        span(gate),
        0.0_f32,
        Color32::from_rgba_unmultiplied(125, 211, 252, 70),
    );

    (rect, response)
}

fn draw_grid(painter: &egui::Painter, rect: Rect, style: &ScopeStyle) {
    let stroke = Stroke::new(1.0_f32, style.grid);
    let n = style.divisions.max(1);

    for i in 1..n {
        let f = i as f32 / n as f32;
        let x = rect.left() + rect.width() * f;
        let y = rect.top() + rect.height() * f;
        painter.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            stroke,
        );
        painter.line_segment(
            [Pos2::new(rect.left(), y), Pos2::new(rect.right(), y)],
            stroke,
        );
    }
}

fn auto_range(samples: &[f32]) -> (f32, f32) {
    let (min, max) = samples
        .iter()
        .fold((f32::MAX, f32::MIN), |(lo, hi), s| (lo.min(*s), hi.max(*s)));
    // Pad so the trace never rides the bezel.
    let pad = ((max - min) * 0.1).max(1e-3);
    (min - pad, max + pad)
}

/// Reduce `samples` to at most `2 * buckets` points, preserving each bucket's
/// minimum and maximum so transients survive.
fn decimate_min_max(samples: &[f32], buckets: usize) -> Vec<f32> {
    if buckets == 0 || samples.len() <= buckets * 2 {
        return samples.to_vec();
    }
    let per = samples.len() / buckets;
    let mut out = Vec::with_capacity(buckets * 2);

    for chunk in samples.chunks(per) {
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for s in chunk {
            lo = lo.min(*s);
            hi = hi.max(*s);
        }
        // Emit in the order they occur so the line does not zig-zag falsely.
        let first_is_low = chunk.first().copied().unwrap_or(lo) <= (lo + hi) * 0.5;
        if first_is_low {
            out.push(lo);
            out.push(hi);
        } else {
            out.push(hi);
            out.push(lo);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimation_preserves_a_lone_spike() {
        let mut samples = vec![0.0f32; 10_000];
        samples[5_000] = 9.9;

        let out = decimate_min_max(&samples, 100);
        assert!(out.len() <= 200);
        assert!(
            out.iter().any(|v| (*v - 9.9).abs() < 1e-6),
            "a transient must survive decimation — that is what a scope is for"
        );
    }

    #[test]
    fn short_signals_pass_through_untouched() {
        let samples = vec![1.0, 2.0, 3.0];
        assert_eq!(decimate_min_max(&samples, 100), samples);
    }

    #[test]
    fn auto_range_pads_a_flat_signal_instead_of_collapsing() {
        let (lo, hi) = auto_range(&[0.5, 0.5, 0.5]);
        assert!(hi > lo, "a DC signal must still get a drawable range");
    }
}
