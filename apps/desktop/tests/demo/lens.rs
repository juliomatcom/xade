//! The camera's lens: crops a fractional window out of a rendered frame and resamples it to the
//! video's size, so a slow zoom moves smoothly and stays sharp.

use std::thread;

use egui::Rect;

const CHANNELS: usize = 4;

/// The source pixels feeding one output pixel along an axis, with their weights.
struct Taps {
    first: usize,
    weights: Vec<f32>,
}

/// Catmull-Rom, widened when shrinking so no source pixel is skipped.
fn taps(start: f32, span: f32, out: usize, limit: usize) -> Vec<Taps> {
    let ratio = span / out as f32;
    let width = ratio.max(1.0);
    (0..out)
        .map(|i| {
            let center = start + (i as f32 + 0.5) * ratio - 0.5;
            let first = (center - 2.0 * width).ceil().max(0.0) as usize;
            let last = ((center + 2.0 * width).floor().max(0.0) as usize).min(limit - 1);
            let first = first.min(last);
            let mut weights: Vec<f32> = (first..=last)
                .map(|s| {
                    let x = ((s as f32 - center) / width).abs();
                    if x < 1.0 {
                        1.5 * x * x * x - 2.5 * x * x + 1.0
                    } else if x < 2.0 {
                        -0.5 * x * x * x + 2.5 * x * x - 4.0 * x + 2.0
                    } else {
                        0.0
                    }
                })
                .collect();
            let sum: f32 = weights.iter().sum();
            for w in &mut weights {
                *w /= sum;
            }
            Taps { first, weights }
        })
        .collect()
}

/// The `window` (in source pixels) of an RGBA image `src_w` wide, resampled to `out` pixels.
pub fn resample(src: &[u8], src_w: usize, window: Rect, out: [usize; 2]) -> Vec<u8> {
    let src_h = src.len() / (src_w * CHANNELS);
    let [w, h] = out;
    let columns = taps(window.min.x, window.width(), w, src_w);
    let rows = taps(window.min.y, window.height(), h, src_h);
    let top = rows[0].first;
    let bottom = rows[h - 1].first + rows[h - 1].weights.len();
    let threads = thread::available_parallelism().map_or(4, |n| n.get());

    // Across, only over the rows the window needs.
    let mut wide = vec![0.0f32; w * (bottom - top) * CHANNELS];
    let band = (bottom - top).div_ceil(threads);
    thread::scope(|scope| {
        for (n, chunk) in wide.chunks_mut(band * w * CHANNELS).enumerate() {
            let columns = &columns;
            scope.spawn(move || {
                for (y, line) in chunk.chunks_mut(w * CHANNELS).enumerate() {
                    let from = (top + n * band + y) * src_w * CHANNELS;
                    for (pixel, t) in line.chunks_mut(CHANNELS).zip(columns) {
                        for (i, weight) in t.weights.iter().enumerate() {
                            let at = from + (t.first + i) * CHANNELS;
                            for c in 0..CHANNELS {
                                pixel[c] += weight * f32::from(src[at + c]);
                            }
                        }
                    }
                }
            });
        }
    });

    // Then down.
    let mut image = vec![0u8; w * h * CHANNELS];
    let band = h.div_ceil(threads);
    thread::scope(|scope| {
        for (n, chunk) in image.chunks_mut(band * w * CHANNELS).enumerate() {
            let (rows, wide) = (&rows, &wide);
            scope.spawn(move || {
                let mut sum = vec![0.0f32; w * CHANNELS];
                for (y, line) in chunk.chunks_mut(w * CHANNELS).enumerate() {
                    let t = &rows[n * band + y];
                    sum.fill(0.0);
                    for (i, weight) in t.weights.iter().enumerate() {
                        let from = (t.first - top + i) * w * CHANNELS;
                        for (s, v) in sum.iter_mut().zip(&wide[from..]) {
                            *s += weight * v;
                        }
                    }
                    for (out, s) in line.iter_mut().zip(&sum) {
                        *out = s.round().clamp(0.0, 255.0) as u8;
                    }
                }
            });
        }
    });
    image
}
