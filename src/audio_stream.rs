use std::time::{Duration, Instant};

pub(crate) const WAV_HEADER_BYTES: usize = 44;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ByteRange {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl ByteRange {
    pub(crate) fn len(self) -> usize {
        self.end - self.start + 1
    }
}

pub(crate) fn bounded_audio_byte_range(
    value: &str,
    total_length: usize,
    maximum_pcm_bytes: usize,
) -> Result<ByteRange, ()> {
    let (unit, requested) = value.trim().split_once('=').ok_or(())?;
    if !unit.eq_ignore_ascii_case("bytes") || requested.contains(',') || total_length == 0 {
        return Err(());
    }
    let (first, last) = requested.split_once('-').ok_or(())?;
    let (start, end) = if first.is_empty() {
        let suffix = last.parse::<usize>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        (total_length.saturating_sub(suffix), total_length - 1)
    } else {
        let start = first.parse::<usize>().map_err(|_| ())?;
        if start >= total_length {
            return Err(());
        }
        let end = if last.is_empty() {
            total_length - 1
        } else {
            last.parse::<usize>().map_err(|_| ())?.min(total_length - 1)
        };
        if end < start {
            return Err(());
        }
        (start, end)
    };

    let first_pcm_byte = start.saturating_sub(WAV_HEADER_BYTES);
    let bounded_end = WAV_HEADER_BYTES
        .saturating_add(first_pcm_byte)
        .saturating_add(maximum_pcm_bytes)
        .saturating_sub(1)
        .min(end);
    Ok(ByteRange {
        start,
        end: bounded_end,
    })
}

pub(crate) fn wait_for_playback_window(
    generated_samples: usize,
    lookahead_samples: usize,
    sample_rate: u32,
    stream_started: Instant,
    is_cancelled: &impl Fn() -> bool,
) -> bool {
    let paced_samples = generated_samples.saturating_sub(lookahead_samples);
    let target = Duration::from_secs_f64(paced_samples as f64 / f64::from(sample_rate));
    loop {
        if is_cancelled() {
            return false;
        }
        let elapsed = stream_started.elapsed();
        if elapsed >= target {
            return true;
        }
        std::thread::sleep((target - elapsed).min(Duration::from_millis(50)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_open_ranges_without_losing_odd_byte_alignment() {
        let range =
            bounded_audio_byte_range("bytes=45-", 100_000, 8_000).expect("valid byte range");
        assert_eq!(range.start, 45);
        assert_eq!(range.end, 8_044);
        assert_eq!(range.len(), 8_000);
    }

    #[test]
    fn rejects_multiple_or_unsatisfied_ranges() {
        assert!(bounded_audio_byte_range("bytes=0-1,4-5", 100, 20).is_err());
        assert!(bounded_audio_byte_range("bytes=100-", 100, 20).is_err());
    }
}
