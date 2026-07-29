use std::sync::{Condvar, Mutex};
use std::time::Duration;

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
    const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(50);

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
            let (next_state, _) = self
                .completed
                .wait_timeout(state, Self::CANCELLATION_POLL_INTERVAL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next_state;
        }
    }

    pub(crate) fn wake_waiters(&self) {
        self.completed.notify_all();
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
        self.render_with(priority, is_cancelled, || {
            render(project, start_sample, end_sample)
        })
    }

    pub(crate) fn render_with<T>(
        &self,
        priority: AudioRenderPriority,
        is_cancelled: &impl Fn() -> bool,
        render: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, AudioRenderError> {
        let _permit = self.acquire(priority, is_cancelled)?;

        if is_cancelled() {
            return Err(AudioRenderError::Cancelled);
        }
        render().map_err(AudioRenderError::Render)
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    #[test]
    fn cancellation_wakes_a_queued_render_without_waiting_for_the_active_render() {
        let renderer = Arc::new(AudioRenderer::default());
        let cancelled = Arc::new(AtomicBool::new(false));
        renderer.occupy_for_test();

        let worker_renderer = Arc::clone(&renderer);
        let worker_cancelled = Arc::clone(&cancelled);
        let (completed, completion) = mpsc::channel();
        let worker = thread::spawn(move || {
            let result = worker_renderer.render_with(
                AudioRenderPriority::Foreground,
                &|| worker_cancelled.load(Ordering::SeqCst),
                || -> Result<(), String> { panic!("a cancelled queued render must not run") },
            );
            completed.send(result).expect("completion receiver");
        });
        renderer.wait_until_queued_for_test(AudioRenderPriority::Foreground);

        cancelled.store(true, Ordering::SeqCst);
        renderer.wake_waiters();
        let result = completion.recv_timeout(Duration::from_secs(1));
        renderer.release_for_test();
        worker.join().expect("queued render worker");

        assert!(matches!(result, Ok(Err(AudioRenderError::Cancelled))));
        assert_eq!(renderer.queued_for_test(AudioRenderPriority::Foreground), 0);
    }
}
