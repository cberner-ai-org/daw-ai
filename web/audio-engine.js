(() => {
  "use strict";

  const AUDIO_RETRY_DELAYS_MS = [250, 500, 1000];
  const SPECTRUM_WINDOW_SECONDS = 64;
  const AUDIO_SEEK_DEBOUNCE_MS = 200;

  window.createDawAiAudioEngine = ({
    state,
    elements,
    api,
    updateTransport,
    renderPlayhead,
    showError,
    reportClientIssue,
    clamp,
  }) => {
    class AudioEngine {
      constructor() {
        this.playbackState = "idle";
        this.playbackGeneration = 0;
        this.playhead = 0;
        this.timer = null;
        this.media = new Audio();
        this.media.preload = "auto";
        this.audioUrl = null;
        this.audioVersion = null;
        this.audioStart = 0;
        this.streamToken = null;
        this.streamAttempt = 0;
        this.retryTimer = null;
        this.retryAttempts = 0;
        this.seekTimer = null;
        this.spectrumLoadGeneration = 0;
        this.spectrumAbortController = null;
        this.spectrumWindows = [];
        this.spectrumLoading = false;
        this.spectrumLoadingStart = null;
        this.spectrumRetryAfter = 0;
        this.analyzerTracks = [];
        this.analyzerFrame = null;
        this.analyzerGeneration = 0;
        this.media.addEventListener("ended", () => {
          if (this.isPlaying && this.media.ended) this.stop(false);
        });
        this.media.addEventListener("playing", () => this.handlePlaybackStarted());
        this.media.addEventListener("error", () => {
          if (this.isActive) {
            const mediaError = this.media.error;
            this.retryPlayback(
              new Error(
                `The browser could not continue the backend audio stream ` +
                  `(code=${mediaError?.code ?? "unknown"}, networkState=${this.media.networkState}, ` +
                  `readyState=${this.media.readyState}, attempt=${this.streamAttempt}).`,
              ),
              this.playbackGeneration,
              this.streamAttempt,
            );
          }
        });
      }

      get project() {
        return state.project;
      }

      get isPlaying() {
        return this.playbackState === "playing";
      }

      get isActive() {
        return this.playbackState !== "idle" || this.seekTimer !== null;
      }

      async initialize() {
        const access = await api("/api/audio-access", {
          headers: { "X-DAW-AI-Audio": "1" },
        });
        if (typeof access?.streamToken !== "string" || access.streamToken.length < 16) {
          throw new Error("The backend returned an invalid audio stream token.");
        }
        this.streamToken = access.streamToken;
      }

      toggle() {
        if (this.playbackState !== "idle") {
          this.stop(true);
          return Promise.resolve();
        }
        return this.start();
      }

      start() {
        window.clearTimeout(this.seekTimer);
        this.seekTimer = null;
        if (!this.project || !this.streamToken || this.playbackState !== "idle") return Promise.resolve();
        if (this.playhead >= this.project.duration - 0.01) this.playhead = 0;
        this.cancelSpectrumLoad();
        this.playbackState = "starting";
        this.playbackGeneration += 1;
        this.retryAttempts = 0;
        const generation = this.playbackGeneration;
        updateTransport();
        return this.startStream(generation);
      }

      startStream(generation) {
        if (generation !== this.playbackGeneration || !this.isActive) return Promise.resolve();
        const streamAttempt = (this.streamAttempt += 1);
        const startMilliseconds = Math.round(this.playhead * 1000);
        this.audioStart = startMilliseconds / 1000;
        this.audioVersion = this.project.version;
        this.audioUrl = `/api/audio-stream/${encodeURIComponent(this.streamToken)}/${this.audioVersion}/${startMilliseconds}?attempt=${streamAttempt}`;
        this.media.src = this.audioUrl;
        this.media.currentTime = 0;
        this.media.load();

        let playback;
        try {
          // Calling play before yielding preserves the initiating user gesture on WebKit.
          playback = this.media.play();
        } catch (error) {
          this.retryPlayback(error, generation, streamAttempt);
          return Promise.resolve();
        }
        window.clearInterval(this.timer);
        this.timer = window.setInterval(() => this.tick(), 16);
        this.tick();
        return Promise.resolve(playback)
          .catch((error) => {
            this.retryPlayback(error, generation, streamAttempt);
          });
      }

      handlePlaybackStarted() {
        if (this.playbackState !== "starting") return;
        this.playbackState = "playing";
        this.updatePosition();
        const hasStartingSpectrum = this.hasSpectrumAt(this.audioStart);
        if (hasStartingSpectrum) {
          this.startAnalyzers();
        } else {
          this.loadTrackSpectrumAt(this.audioStart);
        }
        this.tick();
        updateTransport();
      }

      stop(preservePosition) {
        if (preservePosition && this.playbackState !== "idle") this.updatePosition();
        this.playbackGeneration += 1;
        this.playbackState = "idle";
        window.clearInterval(this.timer);
        this.timer = null;
        window.clearTimeout(this.retryTimer);
        this.retryTimer = null;
        window.clearTimeout(this.seekTimer);
        this.seekTimer = null;
        this.retryAttempts = 0;
        this.media.pause();
        this.media.removeAttribute("src");
        this.media.load();
        this.stopAnalyzers();
        this.audioUrl = null;
        this.audioVersion = null;
        if (!preservePosition) this.playhead = 0;
        updateTransport();
        renderPlayhead();
      }

      seek(time) {
        const wasActive = this.isActive;
        if (wasActive) {
          this.cancelSpectrumLoad();
          this.stop(true);
        }
        this.playhead = clamp(time, 0, this.project?.duration ?? 0);
        this.audioStart = this.playhead;
        renderPlayhead();
        updateTransport();
        if (wasActive) {
          this.seekTimer = window.setTimeout(() => {
            this.seekTimer = null;
            void this.start();
          }, AUDIO_SEEK_DEBOUNCE_MS);
          updateTransport();
        }
      }

      updatePosition() {
        if (!Number.isFinite(this.media.currentTime)) return;
        this.playhead = Math.min(this.project.duration, this.audioStart + this.media.currentTime);
      }

      retryPlayback(error, generation, streamAttempt) {
        if (
          generation !== this.playbackGeneration ||
          streamAttempt !== this.streamAttempt ||
          !this.isActive ||
          this.retryTimer !== null
        ) {
          return;
        }
        if (error?.name === "NotAllowedError" || this.retryAttempts >= AUDIO_RETRY_DELAYS_MS.length) {
          this.stop(true);
          showError(error, "playing backend audio", "Could not play audio: ");
          return;
        }
        this.updatePosition();
        window.clearInterval(this.timer);
        this.timer = null;
        this.playbackState = "starting";
        const delay = AUDIO_RETRY_DELAYS_MS[this.retryAttempts];
        this.retryAttempts += 1;
        this.retryTimer = window.setTimeout(() => {
          this.retryTimer = null;
          if (generation === this.playbackGeneration && this.isActive) {
            void this.refreshAccessAndRestart(generation, streamAttempt);
          }
        }, delay);
        updateTransport();
      }

      async refreshAccessAndRestart(generation, streamAttempt) {
        try {
          await this.initialize();
        } catch (error) {
          this.retryPlayback(error, generation, streamAttempt);
          return;
        }
        if (
          generation === this.playbackGeneration &&
          streamAttempt === this.streamAttempt &&
          this.isActive
        ) {
          await this.startStream(generation);
        }
      }

      tick() {
        if (!this.isActive) return;
        this.updatePosition();
        if (this.playbackState === "starting" && this.media.currentTime > 0) {
          this.handlePlaybackStarted();
          return;
        }
        updateTransport();
        renderPlayhead();
        if (!this.isPlaying) return;
        if (this.retryAttempts > 0 && this.media.currentTime >= 2) this.retryAttempts = 0;
        if (this.playhead >= this.project.duration) {
          this.stop(false);
          return;
        }
        const spectrumWindow = this.spectrumWindowAt(this.playhead);
        if (spectrumWindow && this.analyzerFrame === null) this.startAnalyzers();
        const spectrumDuration = spectrumWindow?.duration ?? 0;
        const spectrumEnd = (spectrumWindow?.start ?? this.playhead) + spectrumDuration;
        if (Date.now() >= this.spectrumRetryAfter) {
          if (!spectrumWindow) {
            this.loadTrackSpectrumAt(this.playhead);
          } else if (!this.spectrumLoading &&
            spectrumWindow &&
            spectrumEnd < this.project.duration - 0.01 &&
            this.playhead > spectrumEnd - Math.min(2.5, spectrumDuration * 0.8)
          ) {
            void this.loadTrackSpectrum(this.project, spectrumEnd);
          }
        }
      }

      loadTrackSpectrumAt(time) {
        const start = this.spectrumRequestStart(time);
        if (this.spectrumLoading && this.spectrumLoadingStart === start) return;
        void this.loadTrackSpectrum(this.project, start);
      }

      async loadTrackSpectrum(project, start) {
        if (!this.streamToken || !project?.tracks.length) return null;
        const generation = (this.spectrumLoadGeneration += 1);
        this.spectrumAbortController?.abort();
        this.spectrumAbortController = new AbortController();
        this.spectrumLoading = true;
        this.spectrumLoadingStart = start;
        this.updateAnalyzerState();
        if (!this.hasSpectrumAt(this.playhead)) {
          this.stopAnalyzers();
        }
        try {
          const response = await fetch(
            `/api/track-spectrum/${encodeURIComponent(this.streamToken)}/${project.version}/${Math.round(start * 1000)}`,
            { cache: "default", signal: this.spectrumAbortController.signal },
          );
          if (response.status === 409) return false;
          if (!response.ok) throw new Error(`Track spectrum render failed with HTTP ${response.status}.`);
          const decoded = this.decodeSpectrum(await response.arrayBuffer());
          if (generation !== this.spectrumLoadGeneration || project.version !== this.project?.version) return false;
          const tracks = decoded.tracks;
          this.spectrumWindows = this.spectrumWindows.filter((window) => window.start !== start);
          this.spectrumWindows.push({
            version: project.version,
            start: decoded.start,
            duration: decoded.duration,
            tracks,
          });
          elements.trackRows.dataset.spectrumCoverage = `${decoded.start}:${decoded.start + decoded.duration}`;
          this.spectrumWindows.sort((left, right) => left.start - right.start);
          this.updateAnalyzerState();
          this.spectrumRetryAfter = 0;
          if (this.isPlaying) this.startAnalyzers();
          return { start: decoded.start, duration: decoded.duration };
        } catch (error) {
          if (error?.name !== "AbortError" && project.version === this.project?.version) {
            this.spectrumRetryAfter = Date.now() + 1000;
            reportClientIssue("warning", error, "loading track analyzers");
          }
          return null;
        } finally {
          if (generation === this.spectrumLoadGeneration) {
            this.spectrumLoading = false;
            this.spectrumLoadingStart = null;
            this.updateAnalyzerState();
          }
        }
      }

      cancelSpectrumLoad() {
        if (!this.spectrumLoading) return;
        this.spectrumLoadGeneration += 1;
        this.spectrumAbortController?.abort();
        this.spectrumAbortController = null;
        this.spectrumLoading = false;
        this.spectrumLoadingStart = null;
        this.updateAnalyzerState();
      }

      invalidateSpectrum() {
        this.spectrumLoadGeneration += 1;
        this.spectrumAbortController?.abort();
        this.spectrumWindows = [];
        this.spectrumLoading = false;
        this.spectrumLoadingStart = null;
        delete elements.trackRows.dataset.spectrumCoverage;
        this.stopAnalyzers();
        this.updateAnalyzerState();
      }

      warmOpeningSpectrum() {
        if (
          state.promptPending ||
          this.isActive ||
          !this.project ||
          !this.streamToken ||
          this.hasSpectrumAt(0)
        ) return;
        void this.loadTrackSpectrum(this.project, 0);
      }

      updateAnalyzerState(time = this.playhead) {
        const ready = this.hasSpectrumAt(time);
        const loading = !ready &&
          this.spectrumLoading &&
          this.spectrumLoadingStart === this.spectrumRequestStart(time);
        const analyzerState = ready ? "ready" : loading ? "loading" : "unavailable";
        elements.trackRows.querySelectorAll(".track-spectrum").forEach((analyzer) => {
          analyzer.classList.toggle("is-loading", loading);
          analyzer.setAttribute("aria-busy", String(loading));
          analyzer.dataset.state = analyzerState;
        });
      }

      hasSpectrumAt(time) {
        return this.spectrumWindowAt(time) !== null;
      }

      spectrumWindowAt(time) {
        return this.spectrumWindows
          .filter((window) =>
            window.version === this.project?.version &&
            time >= window.start &&
            time < window.start + window.duration
          )
          .at(-1) ?? null;
      }

      spectrumRequestStart(time) {
        return Math.floor(Math.max(0, time) / SPECTRUM_WINDOW_SECONDS) * SPECTRUM_WINDOW_SECONDS;
      }

      decodeSpectrum(arrayBuffer) {
        const bytes = new Uint8Array(arrayBuffer);
        const view = new DataView(arrayBuffer);
        if (String.fromCharCode(...bytes.subarray(0, 8)) !== "DAWSPEC1" || bytes.length < 32) {
          throw new Error("The backend returned invalid track spectrum data.");
        }
        const trackCount = view.getUint32(8, true);
        const frameCount = view.getUint32(12, true);
        const start = Number(view.getBigUint64(16, true)) / 1000;
        const frameSamples = view.getUint32(24, true);
        const sampleRate = view.getUint32(28, true);
        if (!trackCount || !frameCount || !frameSamples || !sampleRate || trackCount > 128) {
          throw new Error("The backend returned invalid track spectrum dimensions.");
        }
        let offset = 32;
        const trackIds = [];
        for (let index = 0; index < trackCount; index += 1) {
          if (offset + 8 > bytes.length) throw new Error("The track spectrum header was truncated.");
          trackIds.push(Number(view.getBigUint64(offset, true)));
          offset += 8;
        }
        if (bytes.length !== offset + frameCount * trackCount * 8) {
          throw new Error("The track spectrum frame data was truncated.");
        }
        const tracks = new Map(trackIds.map((trackId) => [trackId, { levels: [] }]));
        for (let frame = 0; frame < frameCount; frame += 1) {
          for (const trackId of trackIds) {
            tracks.get(trackId).levels.push(bytes.slice(offset, offset + 8));
            offset += 8;
          }
        }
        const frameDuration = frameSamples / sampleRate;
        for (const track of tracks.values()) track.frameDuration = frameDuration;
        return { start, duration: frameCount * frameDuration, tracks };
      }

      startAnalyzers() {
        this.stopAnalyzers();
        const window = this.spectrumWindowAt(this.playhead);
        this.updateAnalyzerState();
        if (!this.isPlaying || !window) return;
        for (const trackId of window.tracks.keys()) {
          this.analyzerTracks.push(trackId);
        }
        this.drawAnalyzers();
      }

      stopAnalyzers() {
        window.cancelAnimationFrame(this.analyzerFrame);
        this.analyzerFrame = null;
        this.analyzerGeneration += 1;
        this.analyzerTracks = [];
        elements.trackRows.querySelectorAll(".track-spectrum i").forEach((bar) => {
          bar.style.setProperty("--spectrum-level", 0);
        });
      }

      drawAnalyzers() {
        this.analyzerFrame = null;
        if (!this.isPlaying) return;
        const projectTime = this.audioStart + this.media.currentTime;
        const spectrumWindow = this.spectrumWindowAt(projectTime);
        this.updateAnalyzerState(projectTime);
        if (spectrumWindow && this.analyzerTracks.length === 0) {
          this.analyzerTracks = [...spectrumWindow.tracks.keys()];
        }
        for (const trackId of this.analyzerTracks) {
          const track = spectrumWindow?.tracks.get(trackId);
          const frame = track
            ? track.levels[
                Math.min(
                  track.levels.length - 1,
                  Math.max(0, Math.floor((projectTime - spectrumWindow.start) / track.frameDuration)),
                )
              ]
            : null;
          const bars = elements.trackRows.querySelectorAll(`[data-spectrum-track="${trackId}"] i`);
          bars.forEach((bar, index) => {
            bar.style.setProperty("--spectrum-level", ((frame?.[index] ?? 0) / 255).toFixed(4));
          });
        }
        const frame = Number(elements.trackRows.dataset.spectrumFrame ?? 0) + 1;
        elements.trackRows.dataset.spectrumFrame = String(frame);
        elements.trackRows.dataset.spectrumLagMs = "0";
        this.analyzerFrame = window.requestAnimationFrame(() => this.drawAnalyzers());
      }
    }

    return new AudioEngine();
  };
})();
