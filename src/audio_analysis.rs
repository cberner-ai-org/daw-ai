use crate::model::{Clip, ClipEvent, Project, Track};

pub(crate) const SAMPLE_RATE: u32 = 48_000;
pub(crate) const CHANNEL_COUNT: usize = 2;
pub(crate) const MAX_REGION_SECONDS: f32 = 16.0;
#[cfg(test)]
pub(crate) const MAX_WAV_SECONDS: f32 =
    (u32::MAX - 36) as f32 / (SAMPLE_RATE * CHANNEL_COUNT as u32 * 2) as f32;
const DSP_SETTLING_SECONDS: f32 = MAX_REGION_SECONDS;
const FFT_SIZE: usize = 512;
const FFT_HOP: usize = 256;

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

pub(crate) fn render_project_stems_sample_range(
    project: &Project,
    start_sample: usize,
    end_sample: usize,
) -> Result<Vec<(u64, AudioRegion)>, String> {
    let track_ids = project
        .tracks
        .iter()
        .map(|track| track.id)
        .collect::<Vec<_>>();
    Ok(render_tracks_with_stems_samples(
        project,
        &track_ids,
        start_sample,
        end_sample,
        &mut || false,
    )?
    .tracks)
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
    Ok(render_audio_samples_with_tracks(
        project,
        track_ids,
        playback_start_sample(start),
        playback_end_sample(end),
        &mut || false,
    )?
    .mix)
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
    let data_bytes = u32::try_from(sample_count.saturating_mul(bytes_per_frame))
        .expect("project duration guarantees a valid RIFF payload");
    let riff_bytes = 36_u32
        .checked_add(data_bytes)
        .expect("project duration guarantees a valid RIFF length");
    let mut wav = Vec::with_capacity(44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_bytes.to_le_bytes());
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
    if let Some(error) = crate::surge_presets::headless_render_error(&track.instrument.preset) {
        return Err(error);
    }
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
    engine.set_free_modulator_phases(&track.modulators, start, f64::from(project.bpm))?;
    let mut event_index = midi.partition_point(|event| event.sample < start_sample);
    let frame_count = output.len().div_euclid(CHANNEL_COUNT);
    let mut output_frame = 0;
    while output_frame < frame_count {
        let block_start = start_sample + output_frame;
        let count = crate::surge::BLOCK_SIZE.min(frame_count - output_frame);
        let final_block = output_frame + count == frame_count;
        for event in scheduled_midi_events_for_block(
            &midi,
            &mut event_index,
            block_start,
            count,
            final_block,
        ) {
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

fn scheduled_midi_events_for_block<'a>(
    midi: &'a [ScheduledMidiEvent],
    event_index: &mut usize,
    block_start: usize,
    block_count: usize,
    final_block: bool,
) -> &'a [ScheduledMidiEvent] {
    // Surge accepts MIDI only at block boundaries. Quantize forward except at the render boundary.
    let dispatch_through = if final_block {
        block_start + block_count.saturating_sub(1)
    } else {
        block_start
    };
    let start = *event_index;
    *event_index += midi[start..].partition_point(|event| event.sample <= dispatch_through);
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
                duration: event
                    .duration
                    .min(((f64::from(clip.end) - time) / beat_duration).max(0.0) as f32),
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
    let spectrum = average_stereo_spectrum(&region.samples);
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

fn average_stereo_spectrum(samples: &[f32]) -> Vec<f32> {
    let channels = [
        samples
            .iter()
            .step_by(CHANNEL_COUNT)
            .copied()
            .collect::<Vec<_>>(),
        samples
            .iter()
            .skip(1)
            .step_by(CHANNEL_COUNT)
            .copied()
            .collect::<Vec<_>>(),
    ];
    let mut spectrum = average_spectrum(&channels[0]);
    let right = average_spectrum(&channels[1]);
    for (left, right) in spectrum.iter_mut().zip(right) {
        *left = (*left + right) * 0.5;
    }
    spectrum
}

fn frame_count(sample_count: usize) -> usize {
    sample_count.saturating_sub(1) / FFT_HOP + 1
}

fn frame_power(samples: &[f32], offset: usize) -> Vec<f32> {
    crate::spectrum::power_512(
        (0..FFT_SIZE).map(|index| samples.get(offset + index).copied().unwrap_or(0.0)),
    )
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

    use super::*;

    fn render_region(
        project: &Project,
        track_ids: &[u64],
        start: f32,
        end: f32,
    ) -> Result<AudioRegion, String> {
        Ok(render_region_with_tracks_cancellable(project, track_ids, start, end, || false)?.mix)
    }

    fn render_region_with_tracks(
        project: &Project,
        track_ids: &[u64],
        start: f32,
        end: f32,
    ) -> Result<AudioRegions, String> {
        render_region_with_tracks_cancellable(project, track_ids, start, end, || false)
    }

    fn render_project_region(project: &Project, start: f32) -> Result<(AudioRegion, f32), String> {
        if !start.is_finite() || start < 0.0 || start >= project.duration {
            return Err("playback start must be inside the project".to_owned());
        }
        let start_sample = playback_start_sample(start);
        let project_end_sample = playback_end_sample(project.duration);
        let maximum_samples = (MAX_REGION_SECONDS * SAMPLE_RATE as f32) as usize;
        let end_sample = start_sample
            .saturating_add(maximum_samples)
            .min(project_end_sample);
        let region = render_project_sample_range(project, start_sample, end_sample)?;
        let end = if end_sample == project_end_sample {
            project.duration
        } else {
            sample_time(end_sample)
        };
        Ok((region, end))
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

    #[test]
    fn renders_and_analyzes_a_demo_region() {
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
    fn free_modulator_phase_is_continuous_across_independent_chunks() {
        let mut project = Project::demo();
        let track = &mut project.tracks[1];
        let target = crate::surge::instrument_parameters_for_instrument(&track.instrument)
            .into_iter()
            .find(|parameter| parameter.scene_modulatable)
            .map(|parameter| format!("native:{}", parameter.id))
            .expect("scene-modulatable parameter");
        track.modulators.push(crate::model::Modulator {
            id: 9_900,
            name: "Chunk phase".to_owned(),
            shape: "sine".to_owned(),
            rate: 0.37,
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
        let track_id = track.id;
        let continuous = render_region(&project, &[track_id], 0.0, 4.0).expect("continuous");
        let second = render_region(&project, &[track_id], 2.0, 4.0).expect("second chunk");
        let offset = 2 * SAMPLE_RATE as usize * CHANNEL_COUNT;
        assert!(
            sample_difference(&continuous.samples[offset..], &second.samples) < 0.000_01,
            "free-running Surge LFO changed phase at the chunk boundary"
        );
    }

    #[test]
    fn selected_track_analysis_matches_nonzero_playback() {
        let mut project = Project::demo();
        project.tracks.retain(|track| track.name == "Soft Current");
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

        assert_eq!(
            precise,
            80_000 * SAMPLE_RATE as usize + SAMPLE_RATE as usize / 1_000
        );
        assert_ne!(precise, playback_start_sample(onset as f32));
    }

    #[test]
    fn midi_events_are_never_dispatched_before_their_sample() {
        let block_start = crate::surge::BLOCK_SIZE;
        let midi = [
            ScheduledMidiEvent {
                sample: block_start,
                note_id: 1,
                pitch: 60,
                velocity: 1.0,
                note_on: true,
            },
            ScheduledMidiEvent {
                sample: block_start + 1,
                note_id: 2,
                pitch: 62,
                velocity: 1.0,
                note_on: true,
            },
        ];
        let mut event_index = 0;

        let dispatched = scheduled_midi_events_for_block(
            &midi,
            &mut event_index,
            block_start,
            crate::surge::BLOCK_SIZE,
            false,
        );

        assert_eq!(dispatched.len(), 1);
        assert_eq!(dispatched[0].note_id, 1);
        assert_eq!(event_index, 1);
    }

    #[test]
    fn final_render_block_dispatches_events_inside_the_copied_window() {
        let block_start = crate::surge::BLOCK_SIZE;
        let block_count = 2;
        let midi = [
            ScheduledMidiEvent {
                sample: block_start,
                note_id: 1,
                pitch: 60,
                velocity: 1.0,
                note_on: true,
            },
            ScheduledMidiEvent {
                sample: block_start + block_count - 1,
                note_id: 2,
                pitch: 62,
                velocity: 1.0,
                note_on: true,
            },
            ScheduledMidiEvent {
                sample: block_start + block_count,
                note_id: 3,
                pitch: 64,
                velocity: 1.0,
                note_on: true,
            },
        ];
        let mut event_index = 0;

        let dispatched = scheduled_midi_events_for_block(
            &midi,
            &mut event_index,
            block_start,
            block_count,
            true,
        );

        assert_eq!(
            dispatched
                .iter()
                .map(|event| event.note_id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(event_index, 2);
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
    fn native_effect_graph_is_rendered_by_surge() {
        let mut project = Project::demo();
        let track_id = project
            .tracks
            .iter()
            .find(|track| track.name == "Soft Current")
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

    #[test]
    fn stereo_analysis_preserves_out_of_phase_energy() {
        let samples = (0..FFT_SIZE)
            .flat_map(|index| {
                let value = (2.0 * PI * 440.0 * index as f32 / SAMPLE_RATE as f32).sin();
                [value, -value]
            })
            .collect::<Vec<_>>();
        let region = AudioRegion {
            samples,
            event_count: 0,
            event_onsets: Vec::new(),
        };
        let analysis = analyze(&region);
        assert!(analysis.spectral_centroid_hz > 400.0);
        assert!(analysis.spectral_centroid_hz < 500.0);
    }

    #[test]
    fn clip_boundary_truncates_sustained_note_gates() {
        let mut project = Project::demo();
        {
            let clip = &mut project.tracks[1].clips[0];
            clip.end = clip.start + 0.5;
            clip.events[0].time = 0.0;
            clip.events[0].duration = 16.0;
        }
        let track = &project.tracks[1];
        let clip = &track.clips[0];
        let occurrences = clip_events_in_window(&project, track, clip, 0.0, 1.0);
        assert_eq!(occurrences.len(), 1);
        assert!(occurrences[0].duration <= project.bpm as f32 / 120.0 + 0.000_01);
    }

    #[test]
    fn factory_wavetable_preset_renders_in_headless_engine() {
        let mut project = Project::demo();
        project.tracks[0].instrument.preset = "Factory/FX/Space Adventure 1".to_owned();
        let track_id = project.tracks[0].id;
        let rendered = render_region_with_tracks(&project, &[track_id], 0.0, 1.0)
            .expect("factory wavetable preset render");
        assert!(rendered.mix.samples.iter().any(|sample| sample.abs() > 0.0));
    }

    fn sample_difference(left: &[f32], right: &[f32]) -> f32 {
        left.iter()
            .zip(right)
            .map(|(left, right)| (left - right).abs())
            .sum::<f32>()
            / left.len().max(1) as f32
    }
}
