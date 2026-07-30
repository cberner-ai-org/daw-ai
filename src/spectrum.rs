use std::sync::{Arc, LazyLock};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftNum, FftPlanner};

struct Transform<T> {
    fft: Arc<dyn Fft<T>>,
    window: Vec<T>,
}

impl Transform<f32> {
    fn new(length: usize) -> Self {
        let mut planner = FftPlanner::new();
        Self {
            fft: planner.plan_fft_forward(length),
            window: hann_window(length, |value| value as f32),
        }
    }

    fn power(&self, samples: impl Iterator<Item = f32>) -> Vec<f32> {
        let mut buffer = samples
            .zip(&self.window)
            .map(|(sample, window)| Complex::new(sample * window, 0.0))
            .collect::<Vec<_>>();
        buffer.resize(self.fft.len(), Complex::new(0.0, 0.0));
        self.fft.process(&mut buffer);
        buffer
            .into_iter()
            .take(self.fft.len() / 2 + 1)
            .map(|value| value.norm_sqr() / self.fft.len() as f32)
            .collect()
    }
}

impl Transform<f64> {
    fn new(length: usize) -> Self {
        let mut planner = FftPlanner::new();
        Self {
            fft: planner.plan_fft_forward(length),
            window: hann_window(length, |value| value),
        }
    }

    fn magnitudes(&self, samples: impl Iterator<Item = f64>) -> Vec<f64> {
        let mut buffer = samples
            .zip(&self.window)
            .map(|(sample, window)| Complex::new(sample * window, 0.0))
            .collect::<Vec<_>>();
        buffer.resize(self.fft.len(), Complex::new(0.0, 0.0));
        self.fft.process(&mut buffer);
        buffer
            .into_iter()
            .take(self.fft.len() / 2)
            .map(|value| value.norm() / (self.fft.len() / 2) as f64)
            .collect()
    }
}

fn hann_window<T: FftNum>(length: usize, convert: impl Fn(f64) -> T) -> Vec<T> {
    (0..length)
        .map(|index| {
            convert(
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * index as f64 / (length - 1) as f64).cos(),
            )
        })
        .collect()
}

pub(crate) fn power_512(samples: impl Iterator<Item = f32>) -> Vec<f32> {
    static TRANSFORM: LazyLock<Transform<f32>> = LazyLock::new(|| Transform::<f32>::new(512));
    TRANSFORM.power(samples)
}

pub(crate) fn magnitudes_1024(samples: impl Iterator<Item = f64>) -> Vec<f64> {
    static TRANSFORM: LazyLock<Transform<f64>> = LazyLock::new(|| Transform::<f64>::new(1024));
    TRANSFORM.magnitudes(samples)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_transform_places_a_bin_centered_tone_in_its_bin() {
        let samples =
            (0..512).map(|index| (2.0 * std::f32::consts::PI * 32.0 * index as f32 / 512.0).sin());
        let spectrum = power_512(samples);
        let strongest = spectrum
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(index, _)| index);
        assert_eq!(strongest, Some(32));
    }
}
