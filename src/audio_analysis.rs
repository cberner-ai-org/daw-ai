use std::f32::consts::PI;

use crate::model::{Clip, ClipEvent, Project, Track};

pub(crate) const SAMPLE_RATE: u32 = 16_000;
pub(crate) const CHANNEL_COUNT: usize = 2;
pub(crate) const MAX_REGION_SECONDS: f32 = 16.0;
const DSP_SETTLING_SECONDS: f32 = MAX_REGION_SECONDS;
const FFT_SIZE: usize = 512;
const FFT_HOP: usize = 256;
const MEL_BANDS: usize = 64;

struct ClipOccurrence<'a> {
    event: &'a ClipEvent,
    time: f64,
    duration: f32,
    velocity: f32,
}

struct TrackRenderState<'a> {
    occurrences: Vec<ClipOccurrence<'a>>,
}

pub(crate) struct AudioRegion {
    pub samples: Vec<f32>,
    pub event_count: usize,
    event_onsets: Vec<f32>,
}

pub(crate) struct AudioRegions {
    pub mix: AudioRegion,
    pub tracks: Vec<(u64, AudioRegion)>,
}

impl AudioRegion {
    pub(crate) fn slice(
        &self,
        sample_start: usize,
        sample_end: usize,
        start: f32,
        end: f32,
    ) -> Self {
        let sample_start = sample_start
            .saturating_mul(CHANNEL_COUNT)
            .min(self.samples.len());
        let sample_end = sample_end
            .saturating_mul(CHANNEL_COUNT)
            .clamp(sample_start, self.samples.len());
        let event_onsets = self
            .event_onsets
            .iter()
            .copied()
            .filter(|onset| *onset >= start && *onset < end)
            .collect::<Vec<_>>();
        Self {
            samples: self.samples[sample_start..sample_end].to_vec(),
            event_count: event_onsets.len(),
            event_onsets,
        }
    }
}

pub(crate) struct RegionAnalysis {
    pub peak: f32,
    pub rms: f32,
    pub zero_crossing_rate: f32,
    pub spectral_centroid_hz: f32,
    pub low_energy_ratio: f32,
    pub mid_energy_ratio: f32,
    pub high_energy_ratio: f32,
}

pub(crate) struct MelSpectrogram {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub frames: usize,
    pub minimum_db: f32,
    pub maximum_db: f32,
}

pub(crate) fn render_region(
    project: &Project,
    track_ids: &[u64],
    start: f32,
    end: f32,
) -> Result<AudioRegion, String> {
    if !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end <= start
        || end > project.duration
        || end - start > MAX_REGION_SECONDS
    {
        return Err(format!(
            "analysis range must be inside the project and no longer than {MAX_REGION_SECONDS} seconds"
        ));
    }
    render_tracks_samples(
        project,
        track_ids,
        playback_start_sample(start),
        playback_end_sample(end),
    )
}

pub(crate) fn render_region_with_tracks(
    project: &Project,
    track_ids: &[u64],
    start: f32,
    end: f32,
) -> Result<AudioRegions, String> {
    render_region_with_tracks_cancellable(project, track_ids, start, end, || false)
}

pub(crate) fn render_region_with_tracks_cancellable(
    project: &Project,
    track_ids: &[u64],
    start: f32,
    end: f32,
    mut cancelled: impl FnMut() -> bool,
) -> Result<AudioRegions, String> {
    if !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end <= start
        || end > project.duration
        || end - start > MAX_REGION_SECONDS
    {
        return Err(format!(
            "analysis range must be inside the project and no longer than {MAX_REGION_SECONDS} seconds"
        ));
    }
    render_tracks_with_stems_samples(
        project,
        track_ids,
        playback_start_sample(start),
        playback_end_sample(end),
        &mut cancelled,
    )
}

pub(crate) fn render_project_region(
    project: &Project,
    start: f32,
) -> Result<(AudioRegion, f32), String> {
    if !start.is_finite() || start < 0.0 || start >= project.duration {
        return Err("playback start must be inside the project".to_owned());
    }
    let (region, end_sample) = render_project_samples(project, playback_start_sample(start))?;
    let project_end_sample = playback_end_sample(project.duration);
    let end = if end_sample == project_end_sample {
        project.duration
    } else {
        sample_time(end_sample)
    };
    Ok((region, end))
}

pub(crate) fn render_project_samples(
    project: &Project,
    start_sample: usize,
) -> Result<(AudioRegion, usize), String> {
    let project_end_sample = playback_end_sample(project.duration);
    if start_sample >= project_end_sample {
        return Err("playback start must be inside the project".to_owned());
    }
    let maximum_samples = (MAX_REGION_SECONDS * SAMPLE_RATE as f32) as usize;
    let end_sample = start_sample
        .saturating_add(maximum_samples)
        .min(project_end_sample);
    let region = render_project_sample_range(project, start_sample, end_sample)?;
    Ok((region, end_sample))
}

pub(crate) fn render_project_sample_range(
    project: &Project,
    start_sample: usize,
    end_sample: usize,
) -> Result<AudioRegion, String> {
    let project_end_sample = playback_end_sample(project.duration);
    let maximum_samples = (MAX_REGION_SECONDS * SAMPLE_RATE as f32) as usize;
    if start_sample >= project_end_sample
        || end_sample <= start_sample
        || end_sample > project_end_sample
        || end_sample - start_sample > maximum_samples
    {
        return Err("playback sample range must be inside one project region".to_owned());
    }
    let track_ids = project
        .tracks
        .iter()
        .map(|track| track.id)
        .collect::<Vec<_>>();
    render_tracks_samples(project, &track_ids, start_sample, end_sample)
}

fn render_tracks_samples(
    project: &Project,
    track_ids: &[u64],
    start_sample: usize,
    end_sample: usize,
) -> Result<AudioRegion, String> {
    Ok(
        render_tracks_with_stems_samples(
            project,
            track_ids,
            start_sample,
            end_sample,
            &mut || false,
        )?
        .mix,
    )
}

fn render_tracks_with_stems_samples(
    project: &Project,
    track_ids: &[u64],
    start_sample: usize,
    end_sample: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<AudioRegions, String> {
    let start = sample_time(start_sample);
    let preroll_sample = playback_preroll_sample(project, start_sample);
    let regions = render_audio_samples_with_tracks(
        project,
        track_ids,
        preroll_sample,
        end_sample,
        cancelled,
    )?;
    let sample_start = start_sample - preroll_sample;
    let sample_end = sample_start + end_sample - start_sample;
    let end = sample_time(end_sample);
    Ok(AudioRegions {
        mix: regions.mix.slice(sample_start, sample_end, start, end),
        tracks: regions
            .tracks
            .into_iter()
            .map(|(track_id, region)| {
                (track_id, region.slice(sample_start, sample_end, start, end))
            })
            .collect(),
    })
}

pub(crate) fn playback_sample_count(start: f32, end: f32) -> usize {
    playback_end_sample(end).saturating_sub(playback_start_sample(start))
}

pub(crate) fn playback_start_sample(time: f32) -> usize {
    midi_event_sample(f64::from(time))
}

pub(crate) fn playback_start_sample_milliseconds(milliseconds: u64) -> usize {
    (milliseconds
        .saturating_mul(u64::from(SAMPLE_RATE))
        .saturating_add(500)
        / 1_000) as usize
}

fn playback_end_sample(time: f32) -> usize {
    (f64::from(time) * f64::from(SAMPLE_RATE)).ceil() as usize
}

fn sample_time(sample: usize) -> f32 {
    precise_sample_time(sample) as f32
}

fn midi_event_sample(time: f64) -> usize {
    (time.max(0.0) * f64::from(SAMPLE_RATE)).round() as usize
}

fn precise_sample_time(sample: usize) -> f64 {
    sample as f64 / f64::from(SAMPLE_RATE)
}

#[cfg(test)]
fn render_audio(
    project: &Project,
    track_ids: &[u64],
    start: f32,
    end: f32,
) -> Result<AudioRegion, String> {
    render_audio_samples(
        project,
        track_ids,
        playback_start_sample(start),
        playback_end_sample(end),
    )
}

fn render_audio_samples(
    project: &Project,
    track_ids: &[u64],
    start_sample: usize,
    end_sample: usize,
) -> Result<AudioRegion, String> {
    Ok(
        render_audio_samples_with_tracks(
            project,
            track_ids,
            start_sample,
            end_sample,
            &mut || false,
        )?
        .mix,
    )
}

fn render_audio_samples_with_tracks(
    project: &Project,
    track_ids: &[u64],
    start_sample: usize,
    end_sample: usize,
    cancelled: &mut dyn FnMut() -> bool,
) -> Result<AudioRegions, String> {
    if cancelled() {
        return Err("audio render interrupted".to_owned());
    }
    if track_ids.is_empty() {
        return Err("at least one track ID is required".to_owned());
    }
    if end_sample <= start_sample {
        return Err("audio range must contain at least one sample".to_owned());
    }
    let start = precise_sample_time(start_sample);
    let end = precise_sample_time(end_sample);
    let sample_count = end_sample - start_sample;
    let mut mix = vec![0.0; sample_count.max(1) * CHANNEL_COUNT];
    let mut event_onsets = Vec::new();
    let mut tracks = Vec::with_capacity(track_ids.len());
    for &track_id in track_ids {
        if cancelled() {
            return Err("audio render interrupted".to_owned());
        }
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .ok_or_else(|| format!("track {track_id} does not exist"))?;
        let mut rendered = vec![0.0; mix.len()];
        let mut track_event_onsets = Vec::new();
        if track.muted {
            tracks.push((
                track_id,
                AudioRegion {
                    samples: rendered,
                    event_count: 0,
                    event_onsets: track_event_onsets,
                },
            ));
            continue;
        }
        let render_state = TrackRenderState::new(project, track, start, end);
        render_track(
            project,
            track,
            &render_state,
            start_sample,
            &mut rendered,
            &mut track_event_onsets,
        )?;
        apply_track_gain(track, &mut rendered);
        event_onsets.extend(track_event_onsets.iter().copied());
        sum_samples(&mut mix, &rendered);
        tracks.push((
            track_id,
            AudioRegion {
                samples: rendered,
                event_count: track_event_onsets.len(),
                event_onsets: track_event_onsets,
            },
        ));
    }
    let event_count = event_onsets.len();
    Ok(AudioRegions {
        mix: AudioRegion {
            samples: mix,
            event_count,
            event_onsets,
        },
        tracks,
    })
}

fn sum_samples(output: &mut [f32], input: &[f32]) {
    for (output, input) in output.iter_mut().zip(input) {
        *output += input;
    }
}

pub(crate) fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let mut wav = wav_header(samples.len().div_euclid(CHANNEL_COUNT));
    wav.extend_from_slice(&pcm_bytes(samples));
    wav
}

pub(crate) fn wav_header(sample_count: usize) -> Vec<u8> {
    let bytes_per_frame = CHANNEL_COUNT * 2;
    let data_bytes =
        u32::try_from(sample_count.saturating_mul(bytes_per_frame)).unwrap_or(u32::MAX);
    let mut wav = Vec::with_capacity(44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36_u32.saturating_add(data_bytes)).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&(CHANNEL_COUNT as u16).to_le_bytes());
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * bytes_per_frame as u32).to_le_bytes());
    wav.extend_from_slice(&(bytes_per_frame as u16).to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    wav
}

pub(crate) fn pcm_bytes(samples: &[f32]) -> Vec<u8> {
    let mut pcm_bytes = Vec::with_capacity(samples.len().saturating_mul(2));
    for sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        pcm_bytes.extend_from_slice(&pcm.to_le_bytes());
    }
    pcm_bytes
}

fn render_track(
    project: &Project,
    track: &Track,
    render_state: &TrackRenderState<'_>,
    start_sample: usize,
    output: &mut [f32],
    event_onsets: &mut Vec<f32>,
) -> Result<(), String> {
    let start = precise_sample_time(start_sample);
    let beat_duration = 60.0 / f64::from(project.bpm);
    let mut midi = Vec::new();
    for (sequence, occurrence) in render_state.occurrences.iter().enumerate() {
        let onset = occurrence.time;
        let duration = (f64::from(occurrence.duration) * beat_duration).max(0.01);
        let region_end =
            start + output.len().div_euclid(CHANNEL_COUNT) as f64 / f64::from(SAMPLE_RATE);
        if onset >= start && onset < region_end {
            event_onsets.push(onset as f32);
        }
        let note_id = occurrence
            .event
            .id
            .wrapping_mul(1_000_003)
            .wrapping_add(sequence as u64);
        midi.push(ScheduledMidiEvent {
            sample: midi_event_sample(onset),
            note_id,
            pitch: occurrence.event.pitch,
            velocity: occurrence.velocity,
            note_on: true,
        });
        midi.push(ScheduledMidiEvent {
            sample: midi_event_sample(onset + duration),
            note_id,
            pitch: occurrence.event.pitch,
            velocity: 0.0,
            note_on: false,
        });
    }
    midi.sort_by_key(|event| (event.sample, event.note_on));

    let mut engine = crate::surge::Engine::new(
        &track.instrument,
        &track.effects,
        &track.routing.effect_order,
        &track.modulators,
        track.id,
        SAMPLE_RATE as f32,
    )?;
    engine.set_tempo(f64::from(project.bpm));
    let mut event_index = midi.partition_point(|event| event.sample < start_sample);
    let frame_count = output.len().div_euclid(CHANNEL_COUNT);
    let mut output_frame = 0;
    while output_frame < frame_count {
        let block_start = start_sample + output_frame;
        let count = crate::surge::BLOCK_SIZE.min(frame_count - output_frame);
        let block_end = block_start + count;
        for event in scheduled_midi_events_before(&midi, &mut event_index, block_end) {
            if event.note_on {
                engine.play_note(event.pitch, event.velocity, event.note_id);
            } else {
                engine.release_note(event.pitch, event.note_id);
            }
        }
        let block = engine.process();
        for index in 0..count {
            let output_index = (output_frame + index) * CHANNEL_COUNT;
            output[output_index] = block[0][index];
            output[output_index + 1] = block[1][index];
        }
        output_frame += count;
    }
    Ok(())
}

struct ScheduledMidiEvent {
    sample: usize,
    note_id: u64,
    pitch: u8,
    velocity: f32,
    note_on: bool,
}

fn scheduled_midi_events_before<'a>(
    midi: &'a [ScheduledMidiEvent],
    event_index: &mut usize,
    block_end: usize,
) -> &'a [ScheduledMidiEvent] {
    let start = *event_index;
    *event_index += midi[start..].partition_point(|event| event.sample < block_end);
    &midi[start..*event_index]
}

fn clip_events_in_window<'a>(
    project: &Project,
    _track: &Track,
    clip: &'a Clip,
    window_start: f64,
    window_end: f64,
) -> Vec<ClipOccurrence<'a>> {
    let beat_duration = 60.0 / f64::from(project.bpm);
    let loop_duration = f64::from(clip.loop_beats) * beat_duration;
    if loop_duration <= 0.0 || window_end <= window_start {
        return Vec::new();
    }
    if clip.events.is_empty() {
        return Vec::new();
    }
    let first_cycle = if clip.playback_mode == "once" {
        0
    } else {
        ((((window_start - f64::from(clip.source_start)) / loop_duration).floor() as i64) - 1)
            .max(0)
    };
    let last_cycle = if clip.playback_mode == "once" {
        0
    } else {
        (((window_end - f64::from(clip.source_start)) / loop_duration).floor() as i64).max(0)
    };
    let mut occurrences = Vec::new();
    for cycle in first_cycle..=last_cycle {
        for event in &clip.events {
            let time = f64::from(clip.source_start)
                + cycle as f64 * loop_duration
                + f64::from(event.time) * beat_duration;
            if time < f64::from(clip.start) || time >= f64::from(clip.end) {
                continue;
            }
            if time < window_start - 0.000_001 || time >= window_end - 0.000_001 {
                continue;
            }
            occurrences.push(ClipOccurrence {
                event,
                time,
                duration: event.duration,
                velocity: event.velocity,
            });
        }
    }
    occurrences.sort_by(|left, right| left.time.total_cmp(&right.time));
    occurrences
}

impl<'a> TrackRenderState<'a> {
    fn new(project: &'a Project, track: &'a Track, start: f64, end: f64) -> Self {
        let beat_duration = 60.0 / f64::from(project.bpm);
        let maximum_voice = f64::from(maximum_voice_lifetime(project, track));
        let render_lookback = (start - maximum_voice).max(0.0);
        let mut occurrences = Vec::new();
        for clip in &track.clips {
            let loop_duration = f64::from(clip.loop_beats) * beat_duration;
            if loop_duration <= 0.0 {
                continue;
            }
            let onset_lookback = (render_lookback - loop_duration * 2.0).max(f64::from(clip.start));
            let window_end = end.min(f64::from(clip.end)) + 0.000_002;
            occurrences.extend(clip_events_in_window(
                project,
                track,
                clip,
                onset_lookback,
                window_end,
            ));
        }
        occurrences.sort_by(|left, right| left.time.total_cmp(&right.time));
        Self { occurrences }
    }
}

fn maximum_voice_lifetime(project: &Project, track: &Track) -> f32 {
    let beat_duration = 60.0 / project.bpm as f32;
    track
        .clips
        .iter()
        .flat_map(|clip| &clip.events)
        .map(|event| event.duration * beat_duration + 8.0)
        .fold(0.0_f32, f32::max)
}

fn playback_preroll_seconds(project: &Project) -> f32 {
    let maximum_voice = project
        .tracks
        .iter()
        .map(|track| maximum_voice_lifetime(project, track))
        .fold(0.0_f32, f32::max);
    maximum_voice + DSP_SETTLING_SECONDS
}

fn playback_preroll_sample(project: &Project, start_sample: usize) -> usize {
    let unaligned =
        (precise_sample_time(start_sample) - f64::from(playback_preroll_seconds(project))).max(0.0);
    // Whole seconds keep the audio and control-rate grids absolute even at the 24-hour limit.
    (unaligned.floor() * f64::from(SAMPLE_RATE)) as usize
}

#[cfg(test)]
fn playback_preroll_start(project: &Project, start: f32) -> f32 {
    sample_time(playback_preroll_sample(
        project,
        playback_start_sample(start),
    ))
}

fn project_sample_index(region_start_sample: usize, index: usize) -> u64 {
    region_start_sample.saturating_add(index) as u64
}

fn project_sample_time(region_start_sample: usize, index: usize) -> f64 {
    project_sample_index(region_start_sample, index) as f64 / f64::from(SAMPLE_RATE)
}

fn apply_track_gain(track: &Track, samples: &mut [f32]) {
    for sample in samples {
        *sample *= track.volume;
    }
}

pub(crate) fn analyze(region: &AudioRegion) -> RegionAnalysis {
    let mono = mono_samples(&region.samples);
    let peak = region
        .samples
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0, f32::max);
    let rms = if region.samples.is_empty() {
        0.0
    } else {
        (region
            .samples
            .iter()
            .map(|sample| sample * sample)
            .sum::<f32>()
            / region.samples.len() as f32)
            .sqrt()
    };
    let zero_crossings = mono
        .windows(2)
        .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
        .count();
    let zero_crossing_rate = zero_crossings as f32 / mono.len().max(1) as f32;
    let spectrum = average_spectrum(&mono);
    let total = spectrum.iter().sum::<f32>().max(f32::EPSILON);
    let mut weighted = 0.0;
    let mut low = 0.0;
    let mut mid = 0.0;
    let mut high = 0.0;
    for (bin, power) in spectrum.iter().copied().enumerate() {
        let frequency = bin as f32 * SAMPLE_RATE as f32 / FFT_SIZE as f32;
        weighted += frequency * power;
        if frequency < 250.0 {
            low += power;
        } else if frequency < 2_500.0 {
            mid += power;
        } else {
            high += power;
        }
    }
    RegionAnalysis {
        peak,
        rms,
        zero_crossing_rate,
        spectral_centroid_hz: weighted / total,
        low_energy_ratio: low / total,
        mid_energy_ratio: mid / total,
        high_energy_ratio: high / total,
    }
}

fn mono_samples(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks_exact(CHANNEL_COUNT)
        .map(|frame| (frame[0] + frame[1]) * 0.5)
        .collect()
}

fn average_spectrum(samples: &[f32]) -> Vec<f32> {
    let frame_count = frame_count(samples.len());
    let stride = (frame_count / 64).max(1);
    let mut spectrum = vec![0.0; FFT_SIZE / 2 + 1];
    let mut measured = 0;
    for frame in (0..frame_count).step_by(stride) {
        let powers = frame_power(samples, frame * FFT_HOP);
        for (total, power) in spectrum.iter_mut().zip(powers) {
            *total += power;
        }
        measured += 1;
    }
    if measured > 0 {
        for power in &mut spectrum {
            *power /= measured as f32;
        }
    }
    spectrum
}

pub(crate) fn mel_spectrogram(region: &AudioRegion) -> MelSpectrogram {
    let mono = mono_samples(&region.samples);
    let frames = frame_count(mono.len());
    let filters = mel_filters();
    let mut values = vec![vec![0.0; MEL_BANDS]; frames];
    let mut maximum_db = -120.0_f32;
    for (frame, bands) in values.iter_mut().enumerate() {
        let powers = frame_power(&mono, frame * FFT_HOP);
        for (band, filter) in bands.iter_mut().zip(&filters) {
            let energy = filter
                .iter()
                .map(|(bin, weight)| powers[*bin] * weight)
                .sum::<f32>();
            *band = 10.0 * energy.max(1e-12).log10();
            maximum_db = maximum_db.max(*band);
        }
    }
    let minimum_db = maximum_db - 72.0;
    let width = frames.clamp(128, 1024) as u32;
    let height = (MEL_BANDS * 4) as u32;
    let mut pixels = vec![0_u8; width as usize * height as usize * 3];
    for x in 0..width as usize {
        let frame = x * frames / width as usize;
        for y in 0..height as usize {
            let band = MEL_BANDS - 1 - y * MEL_BANDS / height as usize;
            let normalized = ((values[frame][band] - minimum_db) / 72.0).clamp(0.0, 1.0);
            let color = heat_color(normalized);
            let offset = (y * width as usize + x) * 3;
            pixels[offset..offset + 3].copy_from_slice(&color);
        }
    }
    MelSpectrogram {
        png: encode_png_rgb(width, height, &pixels),
        width,
        height,
        frames,
        minimum_db,
        maximum_db,
    }
}

fn frame_count(sample_count: usize) -> usize {
    sample_count.saturating_sub(1) / FFT_HOP + 1
}

fn frame_power(samples: &[f32], offset: usize) -> Vec<f32> {
    let mut real = vec![0.0; FFT_SIZE];
    let mut imaginary = vec![0.0; FFT_SIZE];
    for (index, value) in real.iter_mut().enumerate() {
        let window = 0.5 - 0.5 * (2.0 * PI * index as f32 / (FFT_SIZE - 1) as f32).cos();
        *value = samples.get(offset + index).copied().unwrap_or(0.0) * window;
    }
    fft(&mut real, &mut imaginary);
    real.into_iter()
        .zip(imaginary)
        .take(FFT_SIZE / 2 + 1)
        .map(|(real, imaginary)| (real * real + imaginary * imaginary) / FFT_SIZE as f32)
        .collect()
}

fn fft(real: &mut [f32], imaginary: &mut [f32]) {
    let length = real.len();
    let mut reversed = 0;
    for index in 1..length {
        let mut bit = length >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            real.swap(index, reversed);
            imaginary.swap(index, reversed);
        }
    }
    let mut size = 2;
    while size <= length {
        let angle = -2.0 * PI / size as f32;
        for start in (0..length).step_by(size) {
            for offset in 0..size / 2 {
                let phase = angle * offset as f32;
                let cosine = phase.cos();
                let sine = phase.sin();
                let even = start + offset;
                let odd = even + size / 2;
                let odd_real = real[odd] * cosine - imaginary[odd] * sine;
                let odd_imaginary = real[odd] * sine + imaginary[odd] * cosine;
                real[odd] = real[even] - odd_real;
                imaginary[odd] = imaginary[even] - odd_imaginary;
                real[even] += odd_real;
                imaginary[even] += odd_imaginary;
            }
        }
        size *= 2;
    }
}

fn mel_filters() -> Vec<Vec<(usize, f32)>> {
    let minimum_mel = hz_to_mel(30.0);
    let maximum_mel = hz_to_mel(SAMPLE_RATE as f32 / 2.0);
    let points = (0..MEL_BANDS + 2)
        .map(|index| {
            let mel =
                minimum_mel + (maximum_mel - minimum_mel) * index as f32 / (MEL_BANDS + 1) as f32;
            ((mel_to_hz(mel) * FFT_SIZE as f32 / SAMPLE_RATE as f32).floor() as usize)
                .min(FFT_SIZE / 2)
        })
        .collect::<Vec<_>>();
    (0..MEL_BANDS)
        .map(|band| {
            let left = points[band].min(FFT_SIZE / 2 - 2);
            let center = points[band + 1].clamp(left + 1, FFT_SIZE / 2 - 1);
            let right = points[band + 2].clamp(center + 1, FFT_SIZE / 2);
            (left..=right)
                .map(|bin| {
                    let weight = if bin <= center {
                        (bin - left) as f32 / (center - left) as f32
                    } else {
                        (right - bin) as f32 / (right - center) as f32
                    };
                    (bin, weight.max(0.0))
                })
                .collect()
        })
        .collect()
}

fn hz_to_mel(hertz: f32) -> f32 {
    2_595.0 * (1.0 + hertz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2_595.0) - 1.0)
}

fn heat_color(value: f32) -> [u8; 3] {
    let stops = [
        [5.0, 4.0, 20.0],
        [49.0, 18.0, 92.0],
        [22.0, 103.0, 145.0],
        [74.0, 190.0, 145.0],
        [247.0, 225.0, 93.0],
    ];
    let position = value * (stops.len() - 1) as f32;
    let index = (position.floor() as usize).min(stops.len() - 2);
    let fraction = position - index as f32;
    let mut color = [0; 3];
    for (channel, value) in color.iter_mut().enumerate() {
        *value = (stops[index][channel] * (1.0 - fraction) + stops[index + 1][channel] * fraction)
            .round() as u8;
    }
    color
}

fn encode_png_rgb(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let row_bytes = width as usize * 3;
    let mut raw = Vec::with_capacity((row_bytes + 1) * height as usize);
    for row in pixels.chunks_exact(row_bytes) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut compressed = vec![0x78, 0x01];
    for (index, block) in raw.chunks(65_535).enumerate() {
        compressed.push(u8::from((index + 1) * 65_535 >= raw.len()));
        let length = block.len() as u16;
        compressed.extend_from_slice(&length.to_le_bytes());
        compressed.extend_from_slice(&(!length).to_le_bytes());
        compressed.extend_from_slice(block);
    }
    compressed.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &header);
    png_chunk(&mut png, b"IDAT", &compressed);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut checksum_input = Vec::with_capacity(4 + data.len());
    checksum_input.extend_from_slice(kind);
    checksum_input.extend_from_slice(data);
    output.extend_from_slice(&crc32(&checksum_input).to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let mut first = 1_u32;
    let mut second = 0_u32;
    for &byte in data {
        first = (first + u32::from(byte)) % 65_521;
        second = (second + first) % 65_521;
    }
    (second << 16) | first
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edit, TrackRole};
    use crate::prompt::Action;

    fn add_native_effect(project: &mut Project, track_index: usize, id: u64, name: &str) {
        project.tracks[track_index].routing.effect_order.push(id);
        project.tracks[track_index]
            .effects
            .push(crate::model::Effect {
                id,
                name: name.to_owned(),
                preset_slot: None,
                mix: 0.5,
                enabled: true,
                parameters: crate::surge::effect_parameter_values(name),
                parameter_overrides: Vec::new(),
                tempo_sync_parameters: Vec::new(),
                deactivated_parameters: Vec::new(),
            });
    }

    #[cfg(any())]
    fn automation_frame_at(project: &Project, track: &Track, time: f32) -> AutomationFrame {
        let time = f64::from(time);
        let render_state = TrackRenderState::new(project, track, time, time + 0.000_01);
        automation_at(project, track, &render_state, time)
    }

    #[test]
    fn once_clip_events_do_not_wrap() {
        let mut project = Project::demo();
        project.bpm = 60;
        let clip = &mut project.tracks[2].clips[0];
        clip.start = 0.0;
        clip.source_start = 0.0;
        clip.end = 8.0;
        clip.loop_beats = 4.0;
        clip.playback_mode = "loop".to_owned();
        let event_id = clip.events[0].id;
        let looped = clip_events_in_window(
            &project,
            &project.tracks[2],
            &project.tracks[2].clips[0],
            0.0,
            8.0,
        )
        .into_iter()
        .filter(|occurrence| occurrence.event.id == event_id)
        .count();
        project.tracks[2].clips[0].playback_mode = "once".to_owned();
        let once = clip_events_in_window(
            &project,
            &project.tracks[2],
            &project.tracks[2].clips[0],
            0.0,
            8.0,
        )
        .into_iter()
        .filter(|occurrence| occurrence.event.id == event_id)
        .count();

        assert_eq!(looped, 2);
        assert_eq!(once, 1);
    }

    #[cfg(any())]
    fn instrument_parameter_at(
        project: &Project,
        track: &Track,
        target: &str,
        base: f32,
        time: f32,
    ) -> f32 {
        let time = f64::from(time);
        let render_state = TrackRenderState::new(project, track, time, time + 0.000_01);
        parameter_at(project, track, &render_state, target, base, time)
    }

    #[test]
    fn renders_analyzes_and_encodes_a_demo_region() {
        let project = Project::demo();
        let region = render_region(&project, &[1, 2, 3], 0.0, 2.0).expect("audio region");
        assert_eq!(
            region.samples.len(),
            SAMPLE_RATE as usize * 2 * CHANNEL_COUNT
        );
        assert!(region.event_count > 0);
        let analysis = analyze(&region);
        for track_id in [1, 2, 3] {
            let track = render_region(&project, &[track_id], 0.0, 2.0).expect("demo track render");
            let track = analyze(&track);
            let (minimum_peak, minimum_rms) = if track_id == 1 {
                (0.03, 0.005)
            } else {
                (0.05, 0.009)
            };
            assert!(
                track.peak > minimum_peak && track.rms > minimum_rms,
                "reset demo track {track_id} was too quiet: peak {}, RMS {}",
                track.peak,
                track.rms
            );
        }
        assert!(
            analysis.peak > 0.15,
            "reset demo peak was {} with RMS {}",
            analysis.peak,
            analysis.rms
        );
        assert!(
            analysis.rms > 0.03,
            "reset demo RMS was {} with peak {}",
            analysis.rms,
            analysis.peak
        );
        assert!(
            analysis.peak < 0.9,
            "reset demo peak clipped at {}",
            analysis.peak
        );
        assert!(analysis.spectral_centroid_hz > 20.0);
        let spectrogram = mel_spectrogram(&region);
        assert!(spectrogram.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(spectrogram.png.len() > 1_000);
        assert_eq!(spectrogram.height, 256);
        assert!(spectrogram.maximum_db > spectrogram.minimum_db);
    }

    #[test]
    fn mix_is_the_transparent_sum_of_surge_track_outputs() {
        let project = Project::demo();
        let track_ids = project
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let mut cancelled = || false;
        let regions = render_audio_samples_with_tracks(
            &project,
            &track_ids,
            0,
            SAMPLE_RATE as usize,
            &mut cancelled,
        )
        .expect("track and mix render");
        let mut expected = vec![0.0; SAMPLE_RATE as usize * CHANNEL_COUNT];
        for track_id in &track_ids {
            let samples = &regions
                .tracks
                .iter()
                .find(|(candidate, _)| candidate == track_id)
                .expect("rendered track")
                .1
                .samples;
            sum_samples(&mut expected, samples);
        }
        assert_eq!(regions.mix.samples, expected);

        let mut above_full_scale = vec![0.8];
        sum_samples(&mut above_full_scale, &[0.8]);
        assert_eq!(above_full_scale, [1.6]);
    }

    #[test]
    fn project_playback_is_bounded_to_one_audio_chunk() {
        let mut project = Project::demo();
        project.duration = 86_400.0;
        let (region, end) = render_project_region(&project, 0.0).expect("playback region");
        assert_eq!(end, MAX_REGION_SECONDS);
        assert_eq!(
            region.samples.len(),
            (MAX_REGION_SECONDS * SAMPLE_RATE as f32) as usize * CHANNEL_COUNT
        );
    }

    #[cfg(any())]
    #[test]
    fn every_surge_xt_starter_patch_generates_audio_for_midi_notes() {
        let mut instrument = Project::demo().tracks[1].instrument.clone();
        for preset in crate::model::SURGE_PRESETS {
            instrument.preset = (*preset).to_owned();
            let mut engine =
                crate::surge::Engine::new(&instrument, &[], &[], &[], 1, SAMPLE_RATE as f32)
                    .expect("Surge XT engine");
            engine.play_note(48, 0.9, 1);
            let energy = (0..128)
                .flat_map(|_| engine.process())
                .flatten()
                .map(f32::abs)
                .sum::<f32>();
            engine.release_note(48, 1);
            assert!(energy > 0.01, "{preset} rendered silence");
        }
    }

    #[test]
    fn factory_sub_preset_survives_a_full_renderer_control_loop() {
        let mut project = Project::demo();
        project.tracks.truncate(1);
        let track_id = project.tracks[0].id;
        project.tracks[0].instrument.preset = "Factory/Basses/Sub 1".to_owned();
        project.tracks[0].effects.clear();
        project.tracks[0].modulators.clear();
        project.tracks[0].routing.effect_order.clear();
        let region =
            render_region(&project, &[track_id], 0.0, 7.0).expect("factory sub preset render");
        assert!(region.samples.iter().any(|sample| sample.abs() > 0.001));
    }

    #[cfg(any())]
    #[test]
    fn dedicated_drum_triggers_use_bright_one_shot_patch_timbres() {
        let render_voice = |preset: &str, trigger: u8, timbre: f32, duration: f32| {
            let mut project = Project::demo();
            project.tracks.truncate(1);
            let track_id = {
                let track = &mut project.tracks[0];
                track.instrument.preset = preset.to_owned();
                track.instrument.timbre = timbre;
                track.instrument.parameter_overrides.clear();
                track.effects.clear();
                track.routing.effect_order.clear();
                track.modulators.clear();
                track.clips[0].loop_beats = 4.0;
                track.clips[0].events.truncate(1);
                track.clips[0].events[0].pitch = trigger;
                track.clips[0].events[0].duration = duration;
                track.id
            };
            analyze(&render_region(&project, &[track_id], 0.0, 1.0).expect("drum render"))
        };

        let snare = render_voice("Surge Snare", 38, 0.78, 0.0625);
        let hat = render_voice("Surge Closed Hat", 42, 1.0, 0.0625);
        assert!(
            snare.spectral_centroid_hz > 1_000.0,
            "snare must be broadband, got {} Hz",
            snare.spectral_centroid_hz
        );
        assert!(
            hat.spectral_centroid_hz > snare.spectral_centroid_hz,
            "hat should be brighter than snare: {} <= {} Hz",
            hat.spectral_centroid_hz,
            snare.spectral_centroid_hz
        );
        assert!(snare.peak > 0.02 && hat.peak > 0.02);

        let short = render_voice("Surge Snare", 38, 0.78, 0.0625);
        let long = render_voice("Surge Snare", 38, 0.78, 1.0);
        assert!((short.rms - long.rms).abs() < 0.000_001);
    }

    #[test]
    fn overlapping_playback_regions_have_identical_pcm() {
        let project = Project::demo();
        let (earlier, _) = render_project_region(&project, 15.5).expect("earlier playback region");
        let (later, _) = render_project_region(&project, 16.0).expect("later playback region");
        let overlap_offset = (0.5 * SAMPLE_RATE as f32) as usize * CHANNEL_COUNT;
        assert_eq!(
            &earlier.samples[overlap_offset..],
            &later.samples[..earlier.samples.len() - overlap_offset]
        );
    }

    #[test]
    fn playback_overlaps_remain_stable_when_preroll_moves() {
        let mut project = Project::demo();
        project.duration = 64.0;
        for track in &mut project.tracks {
            for clip in &mut track.clips {
                clip.end = 64.0;
            }
        }
        assert_ne!(
            playback_preroll_start(&project, 32.0),
            playback_preroll_start(&project, 40.0)
        );
        let (earlier, _) = render_project_region(&project, 32.0).expect("earlier playback region");
        let (later, _) = render_project_region(&project, 40.0).expect("later playback region");
        let overlap_offset = 8 * SAMPLE_RATE as usize * CHANNEL_COUNT;
        let overlap_samples = earlier.samples.len() - overlap_offset;
        let audible_difference = sample_difference(
            &earlier.samples[overlap_offset..],
            &later.samples[..overlap_samples],
        );
        assert!(
            audible_difference < 0.04,
            "overlap mean difference {audible_difference} exceeded the native modulation tolerance"
        );
    }

    #[test]
    fn selected_track_analysis_matches_nonzero_playback() {
        let mut project = Project::demo();
        project.tracks.retain(|track| track.role == TrackRole::Bass);
        let track_id = project.tracks[0].id;
        let (playback, _) = render_project_region(&project, 16.0).expect("nonzero playback render");
        let analysis =
            render_region(&project, &[track_id], 16.0, 32.0).expect("nonzero analysis render");
        let playback = pcm_bytes(&playback.samples);
        let analysis = pcm_bytes(&analysis.samples);
        let differing = playback
            .chunks_exact(2)
            .zip(analysis.chunks_exact(2))
            .filter(|(left, right)| left != right)
            .count();

        assert_eq!(playback.len(), analysis.len());
        assert_eq!(
            differing, 0,
            "selected-track analysis contained {differing} differing samples"
        );
    }

    #[test]
    fn millisecond_restart_chunks_match_continuous_pcm() {
        let project = Project::demo();
        let track_ids = project
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let continuous =
            render_audio(&project, &track_ids, 0.0, project.duration).expect("continuous render");
        let start = 0.274;
        let start_sample = (start * SAMPLE_RATE as f32).round() as usize * CHANNEL_COUNT;
        let (first, next_start) = render_project_region(&project, start).expect("first chunk");
        let (second, _) = render_project_region(&project, next_start).expect("second chunk");
        let joined = first
            .samples
            .iter()
            .chain(&second.samples)
            .copied()
            .collect::<Vec<_>>();
        let joined = pcm_bytes(&joined);
        let continuous = pcm_bytes(&continuous.samples[start_sample..]);
        let differing = joined
            .chunks_exact(2)
            .zip(continuous.chunks_exact(2))
            .filter(|(left, right)| left != right)
            .count();

        assert_eq!(joined.len(), continuous.len());
        assert_eq!(
            differing, 0,
            "millisecond restart contained {differing} differing samples"
        );
    }

    #[test]
    fn late_midi_events_keep_sub_f32_sample_precision() {
        let onset = 80_000.001_f64;
        let precise = midi_event_sample(onset);

        assert_eq!(precise, 80_000 * SAMPLE_RATE as usize + 16);
        assert_ne!(precise, playback_start_sample(onset as f32));
    }

    #[test]
    fn final_partial_block_does_not_dispatch_later_events() {
        let block_start = crate::surge::BLOCK_SIZE;
        let copied_block_end = block_start + 1;
        let midi = [
            ScheduledMidiEvent {
                sample: block_start,
                note_id: 1,
                pitch: 60,
                velocity: 1.0,
                note_on: true,
            },
            ScheduledMidiEvent {
                sample: copied_block_end,
                note_id: 2,
                pitch: 62,
                velocity: 1.0,
                note_on: true,
            },
        ];
        let mut event_index = 0;

        let dispatched = scheduled_midi_events_before(&midi, &mut event_index, copied_block_end);

        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].note_id, 1);
        assert_eq!(event_index, 1);
    }

    #[cfg(any())]
    #[test]
    fn late_playback_chunk_reconstructs_long_modulated_voices() {
        let mut project = Project::demo();
        project.bpm = 60;
        project.duration = 48.0;
        let project_duration = project.duration;
        project.edits.clear();
        project.tracks.retain(|track| track.role == TrackRole::Bass);
        let track = &mut project.tracks[0];
        let track_id = track.id;
        track.effects.clear();
        track.routing.effect_order.clear();
        track.instrument.attack = 0.01;
        track.instrument.release = 5.0;
        track.modulators[0].target = "instrument.pitch".to_owned();
        track.modulators[0].rate = 0.37;
        track.modulators[0].depth = 1.0;
        track.modulators[0].trigger = "free".to_owned();
        track.clips = vec![Clip {
            id: 9_100,
            label: "Long modulated note".to_owned(),
            start: 0.0,
            end: project_duration,
            source_start: 0.0,
            style: "test".to_owned(),
            playback_mode: "loop".to_owned(),
            loop_beats: 16.0,
            events: vec![ClipEvent {
                id: 9_101,
                kind: "note".to_owned(),
                time: 12.0,
                duration: 16.0,
                pitch: 36,
                velocity: 1.0,
            }],
        }];

        let continuous = render_audio(&project, &[track_id], 0.0, project.duration)
            .expect("continuous playback render");
        let (late, end) = render_project_region(&project, 32.0).expect("late playback chunk");
        let offset = 32 * SAMPLE_RATE as usize * CHANNEL_COUNT;

        assert_eq!(end, project.duration);
        assert_eq!(
            pcm_bytes(&continuous.samples[offset..]),
            pcm_bytes(&late.samples)
        );
    }

    #[test]
    fn rejects_unknown_channels_and_oversized_ranges() {
        let project = Project::demo();
        assert!(render_region(&project, &[999], 0.0, 1.0).is_err());
        assert!(render_region(&project, &[1], 0.0, MAX_REGION_SECONDS + 0.1).is_err());
    }

    #[test]
    fn track_volume_does_not_gate_audio_outside_midi_clip_bounds() {
        let project = Project::initial();
        let track = &project.tracks[0];
        let mut samples = vec![1.0, -1.0, 0.5, -0.5];
        apply_track_gain(track, &mut samples);
        assert_eq!(samples, vec![1.0, -1.0, 0.5, -0.5]);
    }

    #[test]
    fn legacy_regional_actions_do_not_change_rendered_audio() {
        let mut project = Project::demo();
        let track_id = project.tracks[2].id;
        let baseline = render_region(&project, &[track_id], 0.0, 2.0).expect("baseline render");
        project.edits.push(Edit {
            id: 9_001,
            operation_id: None,
            start: 0.0,
            end: 2.0,
            prompt: "Legacy regional edit".to_owned(),
            summary: "Legacy regional edit".to_owned(),
            action: Action::Compound {
                actions: vec![
                    Action::Filter {
                        amount: -0.8,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Rhythm {
                        amount: 0.8,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Gain {
                        amount: 0.1,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Mute {
                        target: Some(TrackRole::Chords),
                    },
                ],
            },
        });
        let rendered = render_region(&project, &[track_id], 0.0, 2.0).expect("legacy render");
        assert_eq!(rendered.samples, baseline.samples);
        assert_eq!(rendered.event_count, baseline.event_count);
    }

    #[test]
    #[cfg(any())]
    fn scoped_parameter_automation_changes_only_its_time_range() {
        let mut project = Project::demo();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        let track_id = project.tracks[track_index].id;
        project.edits.push(Edit {
            id: 9_005,
            operation_id: None,
            start: 0.0,
            end: 4.0,
            prompt: "Build the bass level".to_owned(),
            summary: "Automated the bass level".to_owned(),
            action: Action::Timed {
                start: 0.25,
                end: 0.75,
                action: Box::new(Action::Automation {
                    track_id,
                    parameter: "track.volume".to_owned(),
                    curve: "linear",
                    points: vec![
                        AutomationPoint {
                            time: 0.0,
                            value: 0.1,
                        },
                        AutomationPoint {
                            time: 1.0,
                            value: 1.4,
                        },
                    ],
                    target: TrackRole::Bass,
                }),
            },
        });
        let track = &project.tracks[track_index];
        let baseline = track.volume;
        assert!((automation_frame_at(&project, track, 0.5).gain - baseline).abs() < 0.000_01);
        assert!((automation_frame_at(&project, track, 1.0).gain - 0.1).abs() < 0.000_01);
        assert!((automation_frame_at(&project, track, 2.0).gain - 0.75).abs() < 0.000_01);
        assert!(automation_frame_at(&project, track, 2.9).gain > 1.3);
        assert!((automation_frame_at(&project, track, 3.0).gain - baseline).abs() < 0.000_01);
    }

    #[cfg(any())]
    #[test]
    #[cfg(any())]
    fn all_published_instrument_envelope_automation_reaches_render_controls() {
        let mut project = Project::demo();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        let track_id = project.tracks[track_index].id;
        for (id, parameter, value) in [
            (9_101, "instrument.decay", 0.21),
            (9_102, "instrument.sustain", 0.43),
            (9_103, "instrument.output", 0.65),
        ] {
            project.edits.push(Edit {
                id,
                operation_id: None,
                start: 0.0,
                end: 2.0,
                prompt: format!("Automate {parameter}"),
                summary: format!("Automated {parameter}"),
                action: Action::Automation {
                    track_id,
                    parameter: parameter.to_owned(),
                    curve: "linear",
                    points: vec![AutomationPoint { time: 0.0, value }],
                    target: TrackRole::Bass,
                },
            });
        }
        let track = &project.tracks[track_index];
        for (parameter, value) in [
            ("instrument.decay", 0.21),
            ("instrument.sustain", 0.43),
            ("instrument.output", 0.65),
        ] {
            assert!(
                (instrument_parameter_at(&project, track, parameter, 0.9, 1.0) - value).abs()
                    < 0.000_01,
                "{parameter} automation was not applied"
            );
        }
    }

    #[test]
    #[cfg(any())]
    fn automation_targets_only_its_stable_track_id() {
        let mut project = Project::demo();
        let original_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        let mut newer_bass = project.tracks[original_index].clone();
        newer_bass.id = 9_006;
        newer_bass.name = "Second bass".to_owned();
        let newer_id = newer_bass.id;
        project.tracks.push(newer_bass);
        project.edits.push(Edit {
            id: 9_007,
            operation_id: None,
            start: 0.0,
            end: 4.0,
            prompt: "Raise only the second bass".to_owned(),
            summary: "Automated one bass".to_owned(),
            action: Action::Automation {
                track_id: newer_id,
                parameter: "track.volume".to_owned(),
                curve: "linear",
                points: vec![
                    AutomationPoint {
                        time: 0.0,
                        value: 0.1,
                    },
                    AutomationPoint {
                        time: 1.0,
                        value: 1.4,
                    },
                ],
                target: TrackRole::Bass,
            },
        });

        let original = &project.tracks[original_index];
        let newer = project.tracks.last().expect("second bass");
        assert!(
            (automation_frame_at(&project, original, 1.0).gain - original.volume).abs() < 0.000_01
        );
        assert!((automation_frame_at(&project, newer, 1.0).gain - 0.425).abs() < 0.000_01);
    }

    #[cfg(any())]
    #[test]
    #[cfg(any())]
    fn native_modulator_rate_and_depth_automation_reach_surge() {
        let mut project = Project::demo();
        project.tracks.retain(|track| track.role == TrackRole::Bass);
        let track_id = project.tracks[0].id;
        let modulator_id = 9_008;
        let target =
            crate::surge::instrument_parameters_for_instrument(&project.tracks[0].instrument)
                .into_iter()
                .find(|parameter| {
                    parameter.scene_modulatable && parameter.name.ends_with("Filter 1 Cutoff")
                })
                .map(|parameter| format!("native:{}", parameter.id))
                .expect("modulatable native parameter");
        project.tracks[0].modulators.push(Modulator {
            id: modulator_id,
            name: "Native movement".to_owned(),
            shape: "sine".to_owned(),
            rate: 0.2,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            source_track_id: None,
            attack_ms: 0.0,
            release_ms: 10.0,
            threshold: 0.0,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth: 0.8,
            target,
            enabled: true,
        });

        let mut without_modulation = project.clone();
        without_modulation.tracks[0].modulators[0].enabled = false;
        let dry =
            render_region(&without_modulation, &[track_id], 0.0, 2.0).expect("unmodulated render");

        let mut zero_depth = project.clone();
        zero_depth.edits.push(Edit {
            id: 9_009,
            operation_id: None,
            start: 0.0,
            end: 2.0,
            prompt: "Remove movement".to_owned(),
            summary: "Automated native modulation depth".to_owned(),
            action: Action::Automation {
                track_id,
                parameter: format!("modulator:{modulator_id}.depth"),
                curve: "linear",
                points: vec![AutomationPoint {
                    time: 0.0,
                    value: 0.0,
                }],
                target: TrackRole::Bass,
            },
        });
        let zero_depth_audio =
            render_region(&zero_depth, &[track_id], 0.0, 2.0).expect("zero-depth render");
        assert!(
            sample_difference(&dry.samples, &zero_depth_audio.samples) < 0.000_001,
            "native depth automation did not remove the Surge modulation route"
        );

        let fixed =
            render_region(&project, &[track_id], 0.0, 2.0).expect("fixed-rate native render");
        project.edits.push(Edit {
            id: 9_010,
            operation_id: None,
            start: 0.0,
            end: 2.0,
            prompt: "Accelerate movement".to_owned(),
            summary: "Automated native modulation rate".to_owned(),
            action: Action::Automation {
                track_id,
                parameter: format!("modulator:{modulator_id}.rate"),
                curve: "linear",
                points: vec![AutomationPoint {
                    time: 0.0,
                    value: 8.0,
                }],
                target: TrackRole::Bass,
            },
        });
        let fast =
            render_region(&project, &[track_id], 0.0, 2.0).expect("automated-rate native render");
        assert!(
            sample_difference(&fixed.samples, &fast.samples) > 0.000_1,
            "native rate automation did not change the Surge render"
        );
    }

    #[test]
    #[cfg(any())]
    fn release_automation_extends_the_render_lookback() {
        let mut project = Project::demo();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        let track_id = project.tracks[track_index].id;
        project.edits.push(Edit {
            id: 9_009,
            operation_id: None,
            start: 0.0,
            end: 4.0,
            prompt: "Lengthen the bass release".to_owned(),
            summary: "Automated the bass release".to_owned(),
            action: Action::Automation {
                track_id,
                parameter: "instrument.release".to_owned(),
                curve: "hold",
                points: vec![
                    AutomationPoint {
                        time: 0.0,
                        value: 0.8,
                    },
                    AutomationPoint {
                        time: 1.0,
                        value: 0.8,
                    },
                ],
                target: TrackRole::Bass,
            },
        });

        let automation = AutomationIndex::new(&project, &project.tracks[track_index]);
        assert!(maximum_voice_lifetime(&project, &project.tracks[track_index], &automation) >= 8.0);
        let state = TrackRenderState::new(&project, &project.tracks[track_index], 3.0, 3.5);
        assert!(
            state
                .occurrences
                .iter()
                .any(|occurrence| occurrence.time < 3.0)
        );
    }

    #[test]
    #[cfg(any())]
    fn render_state_indexes_only_automation_owned_by_its_track() {
        let mut project = Project::demo();
        let bass_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        for index in 0..256 {
            project.edits.push(Edit {
                id: 10_000 + index,
                operation_id: None,
                start: 0.0,
                end: 2.0,
                prompt: "Unrelated regional edit".to_owned(),
                summary: "Unrelated regional edit".to_owned(),
                action: Action::Gain {
                    amount: 1.0,
                    target: Some(TrackRole::Chords),
                },
            });
        }
        let bass = &project.tracks[bass_index];
        let state = TrackRenderState::new(&project, bass, 0.0, 2.0);
        assert!(state.automation.lanes.is_empty());
        assert_eq!(
            state.automation.value_at("instrument.resonance", 0.0, 1.0),
            0.0
        );
    }

    #[test]
    fn native_effect_graph_is_rendered_by_surge() {
        let mut project = Project::demo();
        let track_id = project
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Bass)
            .expect("demo bass")
            .id;
        project.tracks[1].instrument.preset = "Init".to_owned();
        let surge_baseline =
            render_region(&project, &[track_id], 0.0, 2.0).expect("Surge baseline");

        project.tracks[1].effects.push(crate::model::Effect {
            id: 9_002,
            name: "Distortion".to_owned(),
            preset_slot: None,
            mix: 0.8,
            enabled: true,
            parameters: crate::surge::effect_parameter_values("Distortion"),
            parameter_overrides: Vec::new(),
            tempo_sync_parameters: Vec::new(),
            deactivated_parameters: Vec::new(),
        });
        project.tracks[1].routing.effect_order.push(9_002);
        let surge_driven =
            render_region(&project, &[track_id], 0.0, 2.0).expect("Surge distortion");
        assert!(sample_difference(&surge_driven.samples, &surge_baseline.samples) > 0.01);

        project.tracks[1]
            .effects
            .iter_mut()
            .find(|effect| effect.id == 9_002)
            .expect("distortion effect")
            .enabled = false;
        let surge_bypassed =
            render_region(&project, &[track_id], 0.0, 2.0).expect("bypassed Surge render");
        assert_eq!(surge_bypassed.samples, surge_baseline.samples);
    }

    #[cfg(any())]
    #[test]
    fn native_eq_parameters_render_and_daw_effect_modulation_is_inert() {
        let mut project = Project::demo();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        let track_id = project.tracks[track_index].id;
        project.tracks[track_index].instrument.preset = "Init".to_owned();
        project.tracks[track_index].modulators.clear();
        add_native_effect(&mut project, track_index, 9_004, "Graphic EQ");
        let effect = &mut project.tracks[track_index].effects[0];
        effect.parameter_overrides.push("Gain 1".to_owned());
        effect.parameters.insert("Gain 1".to_owned(), 0.5);
        let neutral = render_region(&project, &[track_id], 0.0, 2.0).expect("neutral filter");

        project.tracks[track_index].effects[0]
            .parameters
            .insert("Gain 1".to_owned(), 1.0);
        let boosted = render_region(&project, &[track_id], 0.0, 2.0).expect("boosted EQ");
        let gain_difference = sample_difference(&boosted.samples, &neutral.samples);
        assert!(
            gain_difference > 0.000_1,
            "EQ gain render difference was {gain_difference}"
        );
        let effect_id = project.tracks[track_index].effects[0].id;
        project.tracks[track_index].modulators.push(Modulator {
            id: 9_003,
            name: "Filter sweep".to_owned(),
            shape: "square".to_owned(),
            rate: 2.0,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            source_track_id: None,
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.1,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth: 0.6,
            target: format!("effect:{effect_id}.Gain 1"),
            enabled: true,
        });
        let modulated = render_region(&project, &[track_id], 0.0, 2.0).expect("modulated filter");
        assert_eq!(modulated.samples, boosted.samples);
    }

    #[cfg(any())]
    #[test]
    fn enabled_modulators_reach_every_listening_parameter() {
        let mut baseline_project = Project::demo();
        let track_index = baseline_project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        baseline_project.tracks[track_index].modulators.clear();
        let track_id = baseline_project.tracks[track_index].id;
        let effect_id = baseline_project.tracks[track_index].effects[0].id;
        let baseline =
            render_region(&baseline_project, &[track_id], 0.0, 1.0).expect("baseline render");

        for target in [
            "instrument.attack".to_owned(),
            "instrument.release".to_owned(),
            "instrument.cutoff".to_owned(),
            "instrument.pitch".to_owned(),
            "instrument.resonance".to_owned(),
            "instrument.pitch".to_owned(),
            "track.volume".to_owned(),
            format!("effect:{effect_id}.Gain 1"),
            format!("effect:{effect_id}.Gain 2"),
            format!("effect:{effect_id}.Gain 3"),
        ] {
            let mut project = baseline_project.clone();
            project.tracks[track_index].modulators.push(Modulator {
                id: 9_002,
                name: "Listening regression".to_owned(),
                shape: "square".to_owned(),
                rate: 0.25,
                rate_mode: "hz".to_owned(),
                trigger: "free".to_owned(),
                source_track_id: None,
                attack_ms: 5.0,
                release_ms: 180.0,
                threshold: 0.1,
                polarity: "increase".to_owned(),
                formula: String::new(),
                depth: 0.8,
                target: target.clone(),
                enabled: true,
            });
            let modulated =
                render_region(&project, &[track_id], 0.0, 1.0).expect("modulated render");
            assert!(
                sample_difference(&modulated.samples, &baseline.samples) > 0.000_01,
                "{target} must affect the listening render"
            );

            project.tracks[track_index].modulators[0].enabled = false;
            let disabled = render_region(&project, &[track_id], 0.0, 1.0).expect("disabled render");
            assert_eq!(disabled.samples, baseline.samples);
        }
    }

    #[cfg(any())]
    #[test]
    fn tempo_sync_scales_with_bpm_and_midi_notes_retrigger_the_listening_modulator() {
        let mut hz_project = Project::demo();
        hz_project.bpm = 120;
        let track_index = hz_project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        let track_id = hz_project.tracks[track_index].id;
        hz_project.tracks[track_index].modulators = vec![Modulator {
            id: 9_003,
            name: "Sync regression".to_owned(),
            shape: "sine".to_owned(),
            rate: 0.25,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            source_track_id: None,
            attack_ms: 5.0,
            release_ms: 180.0,
            threshold: 0.1,
            polarity: "increase".to_owned(),
            formula: String::new(),
            depth: 0.8,
            target: "instrument.cutoff".to_owned(),
            enabled: true,
        }];
        let hz_render = render_region(&hz_project, &[track_id], 0.0, 2.0).expect("Hz render");
        let first_beat = 60.0 / hz_project.bpm as f32;
        let hz_at_first_beat =
            first_modulator_value_at(&hz_project, &hz_project.tracks[track_index], first_beat);

        let mut tempo_project = hz_project.clone();
        tempo_project.tracks[track_index].modulators[0].rate_mode = "tempo".to_owned();
        let tempo_render =
            render_region(&tempo_project, &[track_id], 0.0, 2.0).expect("tempo render");
        let tempo_at_first_beat = first_modulator_value_at(
            &tempo_project,
            &tempo_project.tracks[track_index],
            first_beat,
        );
        assert!((hz_at_first_beat - 0.8 / 2.0_f32.sqrt()).abs() < 0.000_01);
        assert!((tempo_at_first_beat - 0.8).abs() < 0.000_01);
        assert!(sample_difference(&hz_render.samples, &tempo_render.samples) > 0.000_01);

        let mut midi_project = tempo_project.clone();
        midi_project.tracks[track_index].modulators[0].trigger = "midi".to_owned();
        let midi_render =
            render_region(&midi_project, &[track_id], 0.0, 2.0).expect("MIDI-triggered render");
        let midi_at_first_beat =
            first_modulator_value_at(&midi_project, &midi_project.tracks[track_index], first_beat);
        assert!(midi_at_first_beat.abs() < 0.000_01);
        assert!(sample_difference(&tempo_render.samples, &midi_render.samples) > 0.000_01);
    }

    #[cfg(any())]
    #[test]
    fn cross_track_midi_and_audio_sources_drive_target_modulators() {
        let mut project = Project::demo();
        let source_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Drums)
            .expect("drum source");
        let target_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("bass target");
        let source_id = project.tracks[source_index].id;
        let target_id = project.tracks[target_index].id;
        let baseline_project = {
            let mut baseline = project.clone();
            baseline.tracks[target_index].modulators.clear();
            baseline
        };
        let baseline =
            render_region(&baseline_project, &[target_id], 0.0, 2.0).expect("baseline bass");
        let modulator = &mut project.tracks[target_index].modulators[0];
        modulator.target = "track.volume".to_owned();
        modulator.trigger = "midi".to_owned();
        modulator.source_track_id = Some(source_id);
        modulator.shape = "envelope".to_owned();
        modulator.depth = 0.8;
        let midi = render_region(&project, &[target_id], 0.0, 2.0).expect("MIDI-triggered bass");
        assert!(
            sample_difference(&midi.samples, &baseline.samples) > 0.000_01,
            "cross-track MIDI events must affect the target"
        );
        let modulator = &mut project.tracks[target_index].modulators[0];
        modulator.trigger = "audio".to_owned();
        modulator.polarity = "decrease".to_owned();
        modulator.threshold = 0.0;
        modulator.attack_ms = 0.0;
        modulator.release_ms = 200.0;
        modulator.depth = 1.0;
        let ducked = render_region(&project, &[target_id], 0.0, 2.0).expect("audio-ducked bass");
        assert!(
            analyze(&ducked).rms < analyze(&baseline).rms,
            "source audio envelope must reduce target RMS"
        );
    }

    #[test]
    fn cancellable_render_stops_between_tracks() {
        let project = Project::demo();
        let track_ids = project
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();
        let mut checks = 0;
        let result = render_region_with_tracks_cancellable(&project, &track_ids, 0.0, 1.0, || {
            checks += 1;
            checks > 1
        });
        assert!(matches!(result, Err(message) if message == "audio render interrupted"));
    }

    fn sample_difference(left: &[f32], right: &[f32]) -> f32 {
        left.iter()
            .zip(right)
            .map(|(left, right)| (left - right).abs())
            .sum::<f32>()
            / left.len().max(1) as f32
    }
}
