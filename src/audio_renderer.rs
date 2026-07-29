use std::sync::{Condvar, Mutex};

use crate::audio_analysis;
use crate::model::Project;

#[derive(Default)]
struct AudioRenderState {
    rendering: bool,
    foreground_waiters: usize,
    background_waiters: usize,
}

#[derive(Default)]
pub(crate) struct AudioRenderer {
    state: Mutex<AudioRenderState>,
    completed: Condvar,
}

struct AudioWaiter<'a> {
    renderer: &'a AudioRenderer,
    priority: AudioRenderPriority,
    active: bool,
}

struct AudioRenderPermit<'a> {
    renderer: &'a AudioRenderer,
}

#[derive(Debug)]
pub(crate) enum AudioRenderError {
    Render(String),
    Cancelled,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AudioRenderPriority {
    Foreground,
    Background,
}

impl AudioRenderState {
    fn add_waiter(&mut self, priority: AudioRenderPriority) {
        match priority {
            AudioRenderPriority::Foreground => self.foreground_waiters += 1,
            AudioRenderPriority::Background => self.background_waiters += 1,
        }
    }

    fn remove_waiter(&mut self, priority: AudioRenderPriority) {
        match priority {
            AudioRenderPriority::Foreground => self.foreground_waiters -= 1,
            AudioRenderPriority::Background => self.background_waiters -= 1,
        }
    }

    #[cfg(test)]
    fn waiter_count(&self, priority: AudioRenderPriority) -> usize {
        match priority {
            AudioRenderPriority::Foreground => self.foreground_waiters,
            AudioRenderPriority::Background => self.background_waiters,
        }
    }
}

impl Drop for AudioWaiter<'_> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let mut state = self
            .renderer
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.remove_waiter(self.priority);
        self.renderer.completed.notify_all();
    }
}

impl Drop for AudioRenderPermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .renderer
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.rendering = false;
        self.renderer.completed.notify_all();
    }
}

impl AudioRenderer {
    // Queue state transitions happen under `state`; caller callbacks and rendering never do.
    fn acquire(
        &self,
        priority: AudioRenderPriority,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<AudioRenderPermit<'_>, AudioRenderError> {
        if is_cancelled() {
            return Err(AudioRenderError::Cancelled);
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.add_waiter(priority);
        let mut waiter = AudioWaiter {
            renderer: self,
            priority,
            active: true,
        };
        self.completed.notify_all();
        loop {
            drop(state);
            if is_cancelled() {
                return Err(AudioRenderError::Cancelled);
            }
            state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !state.rendering
                && (priority == AudioRenderPriority::Foreground || state.foreground_waiters == 0)
            {
                state.rendering = true;
                state.remove_waiter(priority);
                waiter.active = false;
                drop(state);
                return Ok(AudioRenderPermit { renderer: self });
            }
            state = self
                .completed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(crate) fn stream_sample_range(
        &self,
        project: &Project,
        start_sample: usize,
        end_sample: usize,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<audio_analysis::AudioRegion, AudioRenderError> {
        self.stream_region_with(
            project,
            start_sample,
            end_sample,
            is_cancelled,
            AudioRenderPriority::Foreground,
            audio_analysis::render_project_sample_range,
        )
    }

    pub(crate) fn stream_stems_sample_range(
        &self,
        project: &Project,
        start_sample: usize,
        end_sample: usize,
        is_cancelled: &impl Fn() -> bool,
    ) -> Result<Vec<(u64, audio_analysis::AudioRegion)>, AudioRenderError> {
        self.stream_region_with(
            project,
            start_sample,
            end_sample,
            is_cancelled,
            AudioRenderPriority::Background,
            audio_analysis::render_project_stems_sample_range,
        )
    }

    pub(crate) fn stream_region_with<T>(
        &self,
        project: &Project,
        start_sample: usize,
        end_sample: usize,
        is_cancelled: &impl Fn() -> bool,
        priority: AudioRenderPriority,
        render: impl FnOnce(&Project, usize, usize) -> Result<T, String>,
    ) -> Result<T, AudioRenderError> {
        let _permit = self.acquire(priority, is_cancelled)?;

        if is_cancelled() {
            return Err(AudioRenderError::Cancelled);
        }
        render(project, start_sample, end_sample).map_err(AudioRenderError::Render)
    }

    #[cfg(test)]
    pub(crate) fn occupy_for_test(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rendering = true;
    }

    #[cfg(test)]
    pub(crate) fn release_for_test(&self) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .rendering = false;
        self.completed.notify_all();
    }

    #[cfg(test)]
    pub(crate) fn wait_until_queued_for_test(&self, priority: AudioRenderPriority) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(
            self.completed
                .wait_while(state, |state| state.waiter_count(priority) == 0)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }

    #[cfg(test)]
    pub(crate) fn queued_for_test(&self, priority: AudioRenderPriority) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .waiter_count(priority)
    }
}
