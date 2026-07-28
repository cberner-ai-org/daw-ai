(() => {
  "use strict";

  const elements = {
    skipLink: document.querySelector(".skip-link"),
    projectName: document.querySelector("#project-name"),
    tempo: document.querySelector("#tempo"),
    currentTime: document.querySelector("#current-time"),
    totalTime: document.querySelector("#total-time"),
    playButton: document.querySelector("#play-button"),
    rewindButton: document.querySelector("#rewind-button"),
    timelinePanel: document.querySelector("#timeline-panel"),
    timelineContent: document.querySelector("#timeline-content"),
    timelineScroll: document.querySelector("#timeline-scroll"),
    rulerLane: document.querySelector("#ruler-lane"),
    trackRows: document.querySelector("#track-rows"),
    selection: document.querySelector("#timeline-selection"),
    playhead: document.querySelector("#playhead"),
    selectionReadout: document.querySelector("#selection-readout"),
    selectionModeButton: document.querySelector("#selection-mode-button"),
    promptRange: document.querySelector("#prompt-range"),
    promptForm: document.querySelector("#prompt-form"),
    promptInput: document.querySelector("#prompt-input"),
    composeButton: document.querySelector("#compose-button"),
    editProgress: document.querySelector("#edit-progress"),
    editProgressLabel: document.querySelector("#edit-progress-label"),
    editProgressTime: document.querySelector("#edit-progress-time"),
    editProgressTrack: document.querySelector("#edit-progress-track"),
    editProgressFill: document.querySelector("#edit-progress-fill"),
    undoButton: document.querySelector("#undo-button"),
    aiDurationButton: document.querySelector("#ai-duration-button"),
    resetButton: document.querySelector("#reset-button"),
    savedState: document.querySelector("#saved-state"),
    historyCount: document.querySelector("#history-count"),
    aiModeButton: document.querySelector("#ai-mode-button"),
    aiModePanel: document.querySelector("#ai-mode-panel"),
    debugButton: document.querySelector("#debug-button"),
    debugPanel: document.querySelector("#debug-panel"),
    debugReport: document.querySelector("#debug-report"),
    copyDebug: document.querySelector("#copy-debug"),
    clearDebug: document.querySelector("#clear-debug"),
    batchParameterTools: document.querySelector("#batch-parameter-tools"),
    slimPrompt: document.querySelector("#slim-prompt"),
    dynamicTools: document.querySelector("#dynamic-tools"),
    refreshGeminiSessions: document.querySelector("#refresh-gemini-sessions"),
    geminiSessionList: document.querySelector("#gemini-session-list"),
    sessionHistoryList: document.querySelector("#session-history-list"),
    toast: document.querySelector("#toast"),
    toastMessage: document.querySelector("#toast-message"),
    toastClose: document.querySelector("#toast-close"),
  };

  const state = {
    project: null,
    selectionStart: 8,
    selectionEnd: 16,
    dragPointer: null,
    dragAnchor: 0,
    touchSelectionMode: false,
    longPress: null,
    promptPending: false,
    promptSubmissionClaimed: false,
    activeEditJobId: null,
    interruptPending: false,
    editProgressPercent: 0,
    centeredInitialSelection: false,
    toastTimer: null,
    activeView: "ai",
    clientIssues: [],
    geminiSessions: [],
    projectHistory: { current: 0, entries: [] },
  };
  let historyLoadQueue = Promise.resolve();
  let projectMutationQueue = Promise.resolve();
  const RECONCILED_REQUEST_TIMEOUT_MS = 2000;
  const EDIT_ACCEPTANCE_TIMEOUT_MS = 10_000;
  const PENDING_EDIT_STORAGE_KEY = "daw-ai.pending-edit.v1";
  const BATCH_PARAMETER_TOOLS_STORAGE_KEY = "daw-ai.batch-parameter-tools.v1";
  const SLIM_PROMPT_STORAGE_KEY = "daw-ai.slim-prompt.v1";
  const DYNAMIC_TOOLS_STORAGE_KEY = "daw-ai.dynamic-tools.v1";
  const AUDIO_RETRY_DELAYS_MS = [250, 500, 1000];
  const SPECTRUM_WINDOW_SECONDS = 64;
  const AUDIO_SEEK_DEBOUNCE_MS = 200;
  const TOAST_DISMISS_MS = 4200;
  const ERROR_TOAST_DISMISS_MS = 60_000;
  const LONG_PRESS_MS = 500;
  const LONG_PRESS_MOVE_TOLERANCE_PX = 10;
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
    }

    invalidateSpectrum() {
      this.spectrumLoadGeneration += 1;
      this.spectrumAbortController?.abort();
      this.spectrumWindows = [];
      this.spectrumLoading = false;
      this.spectrumLoadingStart = null;
      this.stopAnalyzers();
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
      if (!this.isPlaying) return;
      const projectTime = this.audioStart + this.media.currentTime;
      const spectrumWindow = this.spectrumWindowAt(projectTime);
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

  const audio = new AudioEngine();

  class ApiError extends Error {
    constructor(message, status, retryable) {
      super(message);
      this.name = "ApiError";
      this.status = status;
      this.retryable = retryable;
    }
  }

  class CommittedEditSyncError extends Error {
    constructor(cause) {
      super(`The edit completed, but the project could not be refreshed. Reload to see it. ${errorMessage(cause)}`);
      this.name = "CommittedEditSyncError";
    }
  }

  function isRetryableHttpStatus(status) {
    return status >= 500 || status === 408 || status === 429;
  }

  async function api(path, options = {}, timeoutMs = null) {
    let requestOptions = { ...options, cache: "no-store" };
    let timeout = null;
    if (timeoutMs !== null) {
      const controller = new AbortController();
      timeout = window.setTimeout(() => controller.abort(), Math.max(1, timeoutMs));
      requestOptions = { ...requestOptions, signal: controller.signal };
    }
    try {
      const response = await fetch(path, requestOptions);
      let data;
      try {
        data = await response.json();
      } catch (_error) {
        throw new ApiError(
          `The studio returned an invalid response (${response.status}).`,
          response.status,
          response.ok || isRetryableHttpStatus(response.status),
        );
      }
      if (!response.ok) {
        throw new ApiError(
          data.error || "The studio could not complete that request.",
          response.status,
          isRetryableHttpStatus(response.status),
        );
      }
      return data;
    } finally {
      if (timeout !== null) window.clearTimeout(timeout);
    }
  }

  function isRetryableApiError(error) {
    return !(error instanceof ApiError) || error.retryable;
  }

  function errorMessage(error) {
    if (error instanceof Error && error.message) return error.message;
    if (typeof error === "string" && error) return error;
    return "Unknown browser error";
  }

  function reportClientIssue(level, error, context) {
    const message = errorMessage(error);
    const stack = error instanceof Error && error.stack ? `\n${error.stack}` : "";
    state.clientIssues.push({
      time: new Date().toISOString(),
      level,
      context: String(context || "browser").slice(0, 160),
      message: `${message}${stack}`.slice(0, 4096),
    });
    state.clientIssues = state.clientIssues.slice(-20);
    renderDebug();
    const body = new URLSearchParams({
      level,
      context: String(context || "browser").slice(0, 160),
      message: `${message}${stack}`.slice(0, 4096),
    });
    void fetch("/api/logs", {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body,
      keepalive: true,
    }).catch(() => {});
  }

  function showError(error, context, prefix = "") {
    reportClientIssue("error", error, context);
    showToast(prefix + errorMessage(error), true);
  }

  function reconcilePlaybackReadiness() {
    const project = state.project;
    elements.playButton.disabled = !project || !audio.streamToken;
    return !elements.playButton.disabled;
  }

  function adoptProject(project) {
    state.project = project;
    audio.playhead = clamp(audio.playhead, 0, project.duration);
    return project;
  }

  async function loadProject() {
    try {
      adoptProject(await api("/api/project"));
      renderProject();
    } catch (error) {
      showError(error, "loading the project");
      elements.savedState.textContent = "Offline";
    }
  }

  function renderProject() {
    const project = state.project;
    if (!project) return;
    elements.sessionHistoryList.dataset.currentEditCount = String(project.edits.length);
    void loadProjectHistory(project.version);
    elements.projectName.textContent = project.name;
    elements.tempo.textContent = project.bpm;
    elements.totalTime.textContent = `/ ${formatTime(project.duration, false)}`;
    elements.savedState.textContent = `Version ${project.version}`;
    elements.undoButton.disabled = !project.canUndo;
    state.selectionStart = clamp(state.selectionStart, 0, project.duration - 0.25);
    state.selectionEnd = clamp(state.selectionEnd, state.selectionStart + 0.25, project.duration);
    renderRuler();
    renderTracks();
    audio.invalidateSpectrum();
    reconcilePlaybackReadiness();
    renderSelection();
    renderPlayhead();
    renderDebug();
    updateTransport();
    if (!state.centeredInitialSelection) {
      state.centeredInitialSelection = true;
      window.requestAnimationFrame(centerSelectionOnNarrowTimeline);
    }
  }

  async function editDuration() {
    if (!state.project) return;
    const entered = window.prompt("Song duration in seconds (1-300)", String(state.project.duration));
    if (entered === null) return;
    const duration = Number(entered);
    if (!Number.isFinite(duration) || duration < 1 || duration > 300) {
      showToast("Enter a duration between 1 second and 5 minutes", true);
      return;
    }
    await enqueueProjectMutation(() => applyProjectMutation({
      path: "/api/duration",
      values: { duration: String(duration) },
      context: "updating the song duration",
      successMessage: "Song duration updated",
    }));
  }

  async function loadProjectHistory(expectedVersion = state.project?.version) {
    const load = historyLoadQueue.then(async () => {
      if (state.project?.version !== expectedVersion) return;
      try {
        const history = await api("/api/history");
        if (history.currentVersion !== expectedVersion || state.project?.version !== expectedVersion) return;
        state.projectHistory = history;
        const changeCount = Math.max(0, state.projectHistory.entries.length - 1);
        elements.historyCount.textContent = `${changeCount} ${changeCount === 1 ? "change" : "changes"}`;
        elements.sessionHistoryList.innerHTML = state.projectHistory.entries
          .slice()
          .reverse()
          .map(
            (entry) => `<button class="history-item" type="button" data-history-index="${entry.index}" data-history-version="${entry.version}" data-history-source="${escapeHtml(entry.source)}" ${entry.index === state.projectHistory.current ? 'aria-current="step"' : ""}><span class="history-marker" aria-hidden="true">${entry.index + 1}</span><span class="history-copy"><span class="history-title"><strong>${escapeHtml(entry.summary)}</strong><em class="history-source history-source-${entry.source.toLowerCase()}">${escapeHtml(entry.source)}</em></span>${entry.prompt ? `<span class="history-prompt">&ldquo;${escapeHtml(entry.prompt)}&rdquo;</span>` : ""}<span>Version ${entry.version}${entry.start == null ? "" : ` &middot; ${entry.start.toFixed(1)} - ${entry.end.toFixed(1)}s`}</span></span><span class="history-current">Current</span></button>`,
          )
          .join("");
      } catch (error) {
        reportClientIssue("warning", error, "loading project history");
      }
    });
    historyLoadQueue = load.catch(() => {});
    return load;
  }

  async function selectProjectHistory(event) {
    const button = event.target.closest("[data-history-index]");
    if (!button) return;
    if (Number(button.dataset.historyIndex) === state.projectHistory.current) return;
    try {
      await replaceProject(async () => {
        adoptProject(await api("/api/history", {
          method: "POST",
          headers: { "Content-Type": "application/x-www-form-urlencoded" },
          body: new URLSearchParams({ index: button.dataset.historyIndex }),
        }));
        renderProject();
      });
      showToast("Project history restored");
    } catch (error) {
      await loadProjectHistory();
      showError(error, "restoring project history");
    }
  }

  function renderRuler() {
    const marks = [];
    const divisions = 16;
    for (let index = 0; index <= divisions; index += 1) {
      const time = (state.project.duration / divisions) * index;
      marks.push(`<span class="ruler-mark" style="left:${(index / divisions) * 100}%">${formatTime(time, false)}</span>`);
    }
    elements.rulerLane.innerHTML = marks.join("");
  }

  function renderTracks() {
    const duration = state.project.duration;
    elements.trackRows.innerHTML = state.project.tracks
      .map((track) => {
        const midiClips = track.clips
          .map((clip) => {
            const left = (clip.start / duration) * 100;
            const width = ((clip.end - clip.start) / duration) * 100;
            return `<div class="clip ${clip.style === "generated" ? "is-generated" : ""} ${track.muted ? "is-muted" : ""}" style="left:${left}%;width:${width}%;--track-color:${track.color}">
              <span class="clip-name">${escapeHtml(clip.label)}</span>
              <span class="timeline-midi" aria-hidden="true">${renderTimelineNotes(track, clip)}</span>
            </div>`;
          })
          .join("");
        const clips = midiClips;
        const markers = state.project.edits
          .filter((edit) => editAppliesToTrack(edit, track))
          .map((edit) => {
            const left = (edit.start / duration) * 100;
            const width = ((edit.end - edit.start) / duration) * 100;
            return `<span class="edit-marker" style="left:${left}%;width:${width}%" title="${escapeHtml(edit.summary)}"></span>`;
          })
          .join("");
        return `<div class="track-row" style="--track-color:${track.color}">
          <div class="track-label">
            <span class="track-color" aria-hidden="true"></span>
            <span class="track-meta"><strong>${escapeHtml(track.name)}</strong><span>${escapeHtml(track.role)}</span></span>
            <span class="track-spectrum" data-spectrum-track="${track.id}" aria-label="${escapeHtml(track.name)} spectrum analyzer">${Array.from({ length: 8 }, () => "<i></i>").join("")}</span>
          </div>
          <div class="track-lane" data-track-id="${track.id}" role="slider" tabindex="0" aria-label="${escapeHtml(track.name)} timeline selection" aria-valuemin="0" aria-valuemax="${duration}" aria-valuenow="${state.selectionStart}" aria-valuetext="Selected ${state.selectionStart.toFixed(1)} to ${state.selectionEnd.toFixed(1)} seconds. Arrow keys move; Shift plus Arrow keys resize.">${clips}${markers}</div>
        </div>`;
      })
      .join("");
  }

  function renderTimelineNotes(track, clip) {
    const playbackBeats = clip.playback?.lengthBeats ?? clip.loopBeats;
    if (clip.events.length === 0 || clip.end <= clip.start || playbackBeats <= 0) return "";
    const clipDuration = clip.end - clip.start;
    const beatDuration = 60 / state.project.bpm;
    const loopDuration = playbackBeats * beatDuration;
    const loopCount = clip.playback?.mode === "once" ? 1 : Math.ceil(clipDuration / loopDuration);
    const occurrenceCount = loopCount * clip.events.length;
    const stride = Math.max(1, Math.ceil(occurrenceCount / 512));
    const pitches = clip.events.map((event) => event.pitch);
    const minimumPitch = Math.min(...pitches);
    const maximumPitch = Math.max(...pitches);
    const pitchSpan = Math.max(1, maximumPitch - minimumPitch);
    const notes = [];
    const renderedOccurrences = Math.min(occurrenceCount, 512);
    for (let renderedIndex = 0; renderedIndex < renderedOccurrences; renderedIndex += 1) {
      const occurrenceIndex = renderedIndex * stride;
      const loop = Math.floor(occurrenceIndex / clip.events.length);
      const event = clip.events[occurrenceIndex % clip.events.length];
      const loopStart = loop * loopDuration;
      const noteStart = loopStart + event.time * beatDuration;
      if (noteStart >= clipDuration) continue;
      const noteDuration = Math.min(event.duration * beatDuration, clipDuration - noteStart);
      const left = (noteStart / clipDuration) * 100;
      const width = Math.max(0.35, (noteDuration / clipDuration) * 100);
      const pitch = (maximumPitch - event.pitch) / pitchSpan;
      const level = track.muted ? 0.06 : clamp(event.velocity * track.volume, 0.08, 1);
      notes.push(
        `<i style="--timeline-note-left:${left}%;--timeline-note-width:${width}%;--timeline-note-pitch:${pitch};--timeline-note-level:${level}"></i>`,
      );
    }
    return notes.join("");
  }

  function renderSelection() {
    if (!state.project) return;
    const laneOffset = elements.rulerLane.offsetLeft;
    const laneWidth = elements.rulerLane.offsetWidth;
    const left = laneOffset + (state.selectionStart / state.project.duration) * laneWidth;
    const width = ((state.selectionEnd - state.selectionStart) / state.project.duration) * laneWidth;
    elements.selection.style.left = `${left}px`;
    elements.selection.style.width = `${Math.max(2, width)}px`;
    elements.selection.style.height = `${elements.trackRows.offsetHeight}px`;
    elements.selectionReadout.textContent = `${state.selectionStart.toFixed(1)}s - ${state.selectionEnd.toFixed(1)}s`;
    elements.promptRange.textContent = `${state.selectionStart.toFixed(1)} - ${state.selectionEnd.toFixed(1)} sec`;
    elements.trackRows.querySelectorAll(".track-lane").forEach((lane) => {
      lane.setAttribute("aria-valuenow", String(state.selectionStart));
      lane.setAttribute(
        "aria-valuetext",
        `Selected ${state.selectionStart.toFixed(1)} to ${state.selectionEnd.toFixed(1)} seconds`,
      );
    });
  }

  function renderPlayhead() {
    if (!state.project) return;
    const left = elements.rulerLane.offsetLeft + (audio.playhead / state.project.duration) * elements.rulerLane.offsetWidth;
    elements.playhead.style.left = `${left}px`;
  }

  function centerSelectionOnNarrowTimeline() {
    const scroll = elements.timelineScroll;
    if (scroll.scrollWidth <= scroll.clientWidth) return;
    const sidebarWidth = elements.rulerLane.offsetLeft;
    const availableWidth = scroll.clientWidth - sidebarWidth;
    const centerTime = (state.selectionStart + state.selectionEnd) / 2;
    const centerPosition = sidebarWidth + (centerTime / state.project.duration) * elements.rulerLane.offsetWidth;
    scroll.scrollLeft = Math.max(0, centerPosition - sidebarWidth - availableWidth / 2);
  }

  async function replaceProject(operation, options = {}) {
    const preservePosition = options.preservePosition !== false;
    const resumePlayback = options.resumePlayback !== false && audio.isActive;
    audio.stop(preservePosition);
    let projectReplaced = false;
    try {
      const result = await operation();
      projectReplaced = true;
      return result;
    } finally {
      const startedDuringReplacement = audio.isActive;
      if (projectReplaced && startedDuringReplacement) audio.stop(preservePosition);
      if ((resumePlayback || startedDuringReplacement) && !audio.isActive) void audio.start();
    }
  }

  function enqueueProjectMutation(operation) {
    const queuedMutation = projectMutationQueue.then(operation, operation);
    projectMutationQueue = queuedMutation.catch(() => {});
    return queuedMutation;
  }

  async function applyProjectMutation({
    path,
    values,
    context,
    successMessage = null,
    restoreUi = null,
    commitUi = null,
    renderOnFailure = false,
  }) {
    try {
      await replaceProject(async () => {
        adoptProject(await api(path, {
          method: "POST",
          headers: { "Content-Type": "application/x-www-form-urlencoded" },
          body: new URLSearchParams(values),
        }));
        commitUi?.();
        renderProject();
        restoreUi?.();
      });
      if (successMessage) showToast(successMessage);
      return true;
    } catch (error) {
      if (renderOnFailure) renderProject();
      restoreUi?.();
      showError(error, context);
      return false;
    }
  }

  function editAppliesToTrack(edit, track) {
    return actionAppliesToTrack(edit.action, track);
  }

  function actionAppliesToTrack(action, track) {
    if (action.type === "compound") return action.actions.some((child) => actionAppliesToTrack(child, track));
    if (action.type === "timed") return actionAppliesToTrack(action.action, track);
    if (action.type === "automation") return action.trackId === track.id;
    return action.target === "all" || action.target === track.role;
  }

  function timelineTimeFromPointer(event) {
    const bounds = elements.rulerLane.getBoundingClientRect();
    const ratio = clamp((event.clientX - bounds.left) / bounds.width, 0, 1);
    return quantize(ratio * state.project.duration, 0.25);
  }

  function beginSelection(event) {
    if (!event.target.closest(".track-lane") || !state.project) return;
    if (event.pointerType === "touch" && !state.touchSelectionMode) {
      cancelLongPress();
      const pointerId = event.pointerId;
      const startX = event.clientX;
      const startY = event.clientY;
      const timer = window.setTimeout(() => {
        if (state.longPress?.pointerId !== pointerId) return;
        state.longPress = null;
        selectWholeTrack();
      }, LONG_PRESS_MS);
      state.longPress = { pointerId, startX, startY, timer };
      return;
    }
    state.dragPointer = event.pointerId;
    state.dragAnchor = timelineTimeFromPointer(event);
    state.selectionStart = Math.min(state.dragAnchor, state.project.duration - 0.25);
    state.selectionEnd = state.selectionStart + 0.25;
    elements.trackRows.setPointerCapture(event.pointerId);
    renderSelection();
  }

  function moveSelection(event) {
    if (state.longPress?.pointerId === event.pointerId) {
      const movement = Math.hypot(
        event.clientX - state.longPress.startX,
        event.clientY - state.longPress.startY,
      );
      if (movement > LONG_PRESS_MOVE_TOLERANCE_PX) cancelLongPress();
    }
    if (event.pointerId !== state.dragPointer) return;
    const current = timelineTimeFromPointer(event);
    if (current === state.dragAnchor) {
      state.selectionStart = Math.min(state.dragAnchor, state.project.duration - 0.25);
      state.selectionEnd = state.selectionStart + 0.25;
      renderSelection();
      return;
    }
    state.selectionStart = Math.min(state.dragAnchor, current);
    state.selectionEnd = Math.max(state.dragAnchor, current);
    renderSelection();
  }

  function endSelection(event) {
    if (state.longPress?.pointerId === event.pointerId) cancelLongPress();
    if (event.pointerId !== state.dragPointer) return;
    state.dragPointer = null;
    if (elements.trackRows.hasPointerCapture(event.pointerId)) {
      elements.trackRows.releasePointerCapture(event.pointerId);
    }
    audio.seek(state.selectionStart);
    renderSelection();
    if (event.pointerType === "touch") setTouchSelectionMode(false);
  }

  function cancelLongPress() {
    if (!state.longPress) return;
    window.clearTimeout(state.longPress.timer);
    state.longPress = null;
  }

  function selectWholeTrack() {
    if (!state.project) return;
    state.selectionStart = 0;
    state.selectionEnd = state.project.duration;
    audio.seek(0);
    renderSelection();
    setTouchSelectionMode(false);
  }

  function selectWholeTrackFromDoubleClick(event) {
    const hit = document.elementFromPoint(event.clientX, event.clientY);
    if (!hit?.closest(".track-lane")) return;
    selectWholeTrack();
  }

  function keepLongPressForTimeline(event) {
    if (event.target.closest(".track-lane")) event.preventDefault();
  }

  function setTouchSelectionMode(enabled) {
    state.touchSelectionMode = enabled;
    elements.trackRows.classList.toggle("is-touch-selecting", enabled);
    elements.selectionModeButton.setAttribute("aria-pressed", String(enabled));
    elements.selectionModeButton.textContent = enabled ? "Drag to select" : "Select region";
  }

  function handleTimelineKey(event) {
    if (!event.target.closest(".track-lane") || !state.project) return;
    const duration = state.project.duration;
    const width = state.selectionEnd - state.selectionStart;
    let handled = true;
    if (event.key === "Home") {
      state.selectionStart = 0;
      state.selectionEnd = width;
    } else if (event.key === "End") {
      state.selectionEnd = duration;
      state.selectionStart = duration - width;
    } else if (event.key === "ArrowLeft" || event.key === "ArrowRight") {
      const change = event.key === "ArrowLeft" ? -0.25 : 0.25;
      if (event.shiftKey) {
        state.selectionEnd = clamp(state.selectionEnd + change, state.selectionStart + 0.25, duration);
      } else {
        state.selectionStart = clamp(state.selectionStart + change, 0, duration - width);
        state.selectionEnd = state.selectionStart + width;
      }
    } else {
      handled = false;
    }
    if (!handled) return;
    event.preventDefault();
    audio.seek(state.selectionStart);
    renderSelection();
  }

  function showEditProgress(job) {
    const elapsed = Math.max(0, Number(job.elapsedSeconds) || 0);
    const timeout = Math.max(1, Number(job.timeoutSeconds) || 20 * 60);
    const detail = job.detail || "The AI producer is working on the edit";
    const appliedSteps = Math.max(0, Number(job.appliedSteps) || 0);
    let nextActivityPercent = 5;
    if (job.status === "completed") {
      nextActivityPercent = 100;
    } else if (job.phase === "syncing") {
      nextActivityPercent = state.editProgressPercent;
    } else if (job.phase === "finalizing") {
      nextActivityPercent = 94;
    } else if (job.phase === "applying") {
      nextActivityPercent = 88;
    } else if (appliedSteps > 0) {
      nextActivityPercent = 90 - 70 / (appliedSteps + 1);
    } else if (job.phase === "planning") {
      nextActivityPercent = 14;
    }
    state.editProgressPercent = Math.max(state.editProgressPercent, nextActivityPercent);
    elements.editProgress.hidden = false;
    if (elements.editProgressLabel.textContent !== detail) elements.editProgressLabel.textContent = detail;
    elements.editProgressTime.textContent = `${formatTime(elapsed, false)} / ${formatTime(timeout, false)}`;
    elements.editProgressFill.style.width = `${state.editProgressPercent}%`;
    elements.editProgressTrack.setAttribute(
      "aria-valuetext",
      appliedSteps > 0 ? `${appliedSteps} edit ${appliedSteps === 1 ? "step" : "steps"} applied. ${detail}` : detail,
    );
    elements.editProgressTrack.removeAttribute("aria-valuenow");
    elements.savedState.textContent = `${detail} - ${formatTime(elapsed, false)} elapsed`;
    elements.composeButton.querySelector("span").textContent = state.promptPending
      ? state.interruptPending
        ? "Interrupting..."
        : "Interrupt"
      : "Make change";
  }

  function hideEditProgress() {
    elements.editProgress.hidden = true;
    state.editProgressPercent = 0;
    elements.editProgressFill.style.width = "0%";
  }

  function wait(milliseconds) {
    return new Promise((resolve) => window.setTimeout(resolve, milliseconds));
  }

  function operationId() {
    if (typeof window.crypto.randomUUID === "function") return window.crypto.randomUUID();
    const bytes = window.crypto.getRandomValues(new Uint8Array(16));
    return `client-${Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("")}`;
  }

  function readPendingEdit() {
    try {
      const serialized = window.localStorage.getItem(PENDING_EDIT_STORAGE_KEY);
      if (!serialized) return null;
      const pending = JSON.parse(serialized);
      const validOperationId =
        typeof pending.operationId === "string" && /^[A-Za-z0-9_-]{1,128}$/.test(pending.operationId);
      const validRequest =
        typeof pending.prompt === "string" &&
        pending.prompt.length > 0 &&
        typeof pending.submittedText === "string" &&
        Number.isFinite(pending.start) &&
        Number.isFinite(pending.end) &&
        pending.start < pending.end;
      const validJob =
        pending.acceptedJob === null ||
        (typeof pending.acceptedJob === "object" &&
          typeof pending.acceptedJob.id === "string" &&
          pending.acceptedJob.operationId === pending.operationId);
      const validBatchSetting =
        pending.batchParameterTools === undefined || typeof pending.batchParameterTools === "boolean";
      const validPromptSetting =
        pending.slimPrompt === undefined || typeof pending.slimPrompt === "boolean";
      const validDynamicSetting =
        pending.dynamicTools === undefined || typeof pending.dynamicTools === "boolean";
      if (validOperationId && validRequest && validJob && validBatchSetting && validPromptSetting && validDynamicSetting) {
        pending.batchParameterTools = pending.batchParameterTools === true;
        pending.slimPrompt = pending.slimPrompt === true;
        pending.dynamicTools = pending.dynamicTools === true;
        return pending;
      }
    } catch (_error) {
      // Invalid or unavailable storage must not prevent the studio from loading.
    }
    try {
      window.localStorage.removeItem(PENDING_EDIT_STORAGE_KEY);
    } catch (_error) {
      // Ignore unavailable storage.
    }
    return null;
  }

  function persistPendingEdit(pending) {
    try {
      window.localStorage.setItem(PENDING_EDIT_STORAGE_KEY, JSON.stringify(pending));
      return true;
    } catch (error) {
      reportClientIssue("warning", error, "persisting an active edit");
      return false;
    }
  }

  function clearPendingEdit(clientOperationId) {
    try {
      const serialized = window.localStorage.getItem(PENDING_EDIT_STORAGE_KEY);
      if (!serialized) return;
      const pending = JSON.parse(serialized);
      if (pending.operationId === clientOperationId) {
        window.localStorage.removeItem(PENDING_EDIT_STORAGE_KEY);
      }
    } catch (_error) {
      // Ignore unavailable storage.
    }
  }

  function requestTimeout(deadline) {
    return Math.min(RECONCILED_REQUEST_TIMEOUT_MS, Math.max(0, Math.floor(deadline - performance.now())));
  }

  async function acceptEdit(clientOperationId, requestBody, onFirstAttempt) {
    const deadline = performance.now() + EDIT_ACCEPTANCE_TIMEOUT_MS;
    let failures = 0;
    let firstAttempt = true;
    for (;;) {
      const timeout = requestTimeout(deadline);
      if (timeout === 0) {
        return {
          status: "unavailable",
          operationId: clientOperationId,
          error: new Error("The studio did not confirm whether it accepted the edit."),
        };
      }
      try {
        const job = await enqueueProjectMutation(async () => {
          if (firstAttempt) {
            firstAttempt = false;
            onFirstAttempt();
          }
          return api(
            "/api/edits",
            {
              method: "POST",
              headers: { "Content-Type": "application/x-www-form-urlencoded" },
              body: requestBody,
            },
            timeout,
          );
        });
        if (job.operationId !== clientOperationId) {
          return {
            status: "unavailable",
            operationId: clientOperationId,
            error: new Error("The studio returned a different edit operation."),
          };
        }
        return job;
      } catch (error) {
        if (error instanceof ApiError && !error.retryable) {
          if (failures === 0) throw error;
          return { status: "unavailable", operationId: clientOperationId, error };
        }
        if (performance.now() >= deadline) {
          return { status: "unavailable", operationId: clientOperationId, error };
        }
        failures += 1;
        showEditProgress({
          phase: "queued",
          detail: "Connection interrupted; confirming the edit was accepted",
          elapsedSeconds: Math.floor((EDIT_ACCEPTANCE_TIMEOUT_MS - (deadline - performance.now())) / 1000),
          timeoutSeconds: EDIT_ACCEPTANCE_TIMEOUT_MS / 1000,
        });
        await wait(clamp(250 * 2 ** (failures - 1), 250, 5000));
      }
    }
  }

  async function pollAcceptedEdit(initialJob) {
    let job = initialJob;
    let consecutivePollFailures = 0;
    const remainingSeconds = Math.max(
      0,
      (Number(initialJob.timeoutSeconds) || 20 * 60) - (Number(initialJob.elapsedSeconds) || 0),
    );
    const visibilityDeadline = performance.now() + remainingSeconds * 1000 + 30_000;
    for (;;) {
      showEditProgress(job);
      const publishedVersion = Number(job.projectVersion);
      if (Number.isFinite(publishedVersion) && publishedVersion > (state.project?.version ?? 0)) {
        try {
          await refreshAuthoritativeProject(
            `Showing Gemini step ${Number(job.appliedSteps) || 1}`,
            Math.min(visibilityDeadline, performance.now() + RECONCILED_REQUEST_TIMEOUT_MS),
          );
          showEditProgress(job);
        } catch (_error) {
          job = { ...job, detail: "Gemini applied a step; retrying the project refresh" };
          showEditProgress(job);
        }
      }
      if (job.status === "completed") return job;
      if (job.status === "failed") return job;
      if (job.status !== "queued" && job.status !== "running") {
        return {
          ...job,
          status: "unavailable",
          error: new Error("The studio returned an unknown edit status."),
        };
      }
      const serverPollAfter = clamp(Number(job.pollAfterMs) || 1000, 20, 5000);
      const pollAfter = clamp(serverPollAfter * 2 ** consecutivePollFailures, 20, 5000);
      await wait(pollAfter);
      const timeout = requestTimeout(visibilityDeadline);
      if (timeout === 0) {
        return {
          ...job,
          status: "unavailable",
          error: new Error("The edit status polling deadline expired."),
        };
      }
      try {
        const nextJob = await api(`/api/edits/${encodeURIComponent(job.id)}`, {}, timeout);
        if (nextJob.operationId !== initialJob.operationId) {
          return {
            ...job,
            status: "unavailable",
            error: new Error("The edit job identity changed."),
          };
        }
        job = nextJob;
        consecutivePollFailures = 0;
      } catch (error) {
        if (!isRetryableApiError(error) || performance.now() >= visibilityDeadline) {
          return { ...job, status: "unavailable", error };
        }
        consecutivePollFailures += 1;
        job = {
          ...job,
          detail: "Connection interrupted; still waiting for the accepted edit",
          elapsedSeconds: (Number(job.elapsedSeconds) || 0) + Math.ceil(pollAfter / 1000),
        };
      }
    }
  }

  async function refreshAuthoritativeProject(detail, deadline = performance.now() + 30_000) {
    let failures = 0;
    for (;;) {
      showEditProgress({
        phase: "syncing",
        detail: failures === 0 ? detail : "Connection interrupted; retrying the project refresh",
        elapsedSeconds: 0,
        timeoutSeconds: 30,
      });
      if (requestTimeout(deadline) === 0) throw new Error("The project refresh deadline expired.");
      try {
        return await enqueueProjectMutation(() =>
          replaceProject(async () => {
            const timeout = requestTimeout(deadline);
            if (timeout === 0) throw new Error("The project refresh deadline expired.");
            adoptProject(await api("/api/project", {}, timeout));
            renderProject();
            return state.project;
          }),
        );
      } catch (error) {
        if (!isRetryableApiError(error) || performance.now() >= deadline) throw error;
        const retryAfter = clamp(250 * 2 ** failures, 250, 5000);
        failures += 1;
        await wait(retryAfter);
      }
    }
  }

  function persistedOperationOutcome(operation) {
    const completed = operation.status === "completed";
    return {
      id: "recovered",
      operationId: operation.operationId,
      status: completed ? "completed" : operation.status || "failed_with_changes",
      phase: completed ? "completed" : "failed",
      message: completed ? operation.message : undefined,
      error: completed
        ? undefined
        : operation.message || "The AI producer stopped before completing the edit.",
      errorStatus: completed ? undefined : 500,
      appliedSteps: Number(operation.appliedSteps) || 0,
      initialVersion: Number(operation.initialVersion) || null,
      projectVersion: Number(operation.projectVersion) || null,
    };
  }

  async function reconcileUnavailableOperation(operationId) {
    const deadline = performance.now() + 2_000;
    for (;;) {
      try {
        const outcome = await api(
          `/api/edit-operations/${encodeURIComponent(operationId)}`,
          {},
          requestTimeout(deadline),
        );
        const publishedVersion = Number(outcome.projectVersion);
        if (Number.isFinite(publishedVersion) && publishedVersion > (state.project?.version ?? 0)) {
          await refreshAuthoritativeProject("Edit status recovered; refreshing the project", deadline);
        }
        return outcome;
      } catch (error) {
        if (!(error instanceof ApiError && error.status === 404) && !isRetryableApiError(error)) {
          throw error;
        }
      }
      let project;
      try {
        project = await refreshAuthoritativeProject(
          "Edit status unavailable; checking the current project",
          deadline,
        );
      } catch (error) {
        if (performance.now() >= deadline) return null;
        throw error;
      }
      const operation = project.editOperations?.find(
        (candidate) => candidate.operationId === operationId,
      );
      if (operation) return persistedOperationOutcome(operation);
      const committedEdit = project.edits.find((edit) => edit.operationId === operationId);
      if (committedEdit) {
        return persistedOperationOutcome({
          operationId,
          status: "completed",
          appliedSteps: 1,
          projectVersion: project.version,
          message: committedEdit.summary,
        });
      }
      if (performance.now() >= deadline) return null;
      await wait(100);
    }
  }

  function appliedEditSteps(outcome) {
    const steps = Number(outcome.appliedSteps);
    return Number.isFinite(steps) ? Math.max(0, Math.floor(steps)) : 0;
  }

  function partialEditError(outcome, refreshError = null) {
    const steps = appliedEditSteps(outcome);
    const rawReason =
      outcome.status === "unavailable"
        ? "The edit status was lost."
        : `${outcome.error || "Gemini could not complete the edit."}`.trim();
    const reason = /[.!?]$/.test(rawReason) ? rawReason : `${rawReason}.`;
    const savedChanges = steps === 1 ? "1 partial change was saved" : `${steps} partial changes were saved`;
    const refreshWarning = refreshError
      ? ` Reload to see the latest saved state. ${errorMessage(refreshError)}`
      : "";
    return new Error(`${reason} ${savedChanges}; review the project before retrying.${refreshWarning}`);
  }

  async function resolveEditOutcome(outcome) {
    if (outcome.status === "completed") {
      return { kind: "completed", message: outcome.message, refresh: true };
    }

    const hasPublishedChanges = appliedEditSteps(outcome) > 0;
    if (outcome.status === "unavailable") {
      if (hasPublishedChanges) return { kind: "partial", error: partialEditError(outcome) };
      return {
        kind: "failed",
        error: new Error("The edit status was lost. The current project was refreshed; review it before retrying."),
      };
    }

    if (
      outcome.status === "failed" ||
      outcome.status === "failed_with_changes" ||
      outcome.status === "interrupted_with_changes"
    ) {
      if (hasPublishedChanges) {
        let refreshError = null;
        try {
          await refreshAuthoritativeProject("The AI producer stopped; refreshing its partial changes");
        } catch (error) {
          refreshError = error;
        }
        return { kind: "partial", error: partialEditError(outcome, refreshError) };
      }
      if (Number(outcome.errorStatus) === 409) {
        await refreshAuthoritativeProject("The project changed; loading its current version");
      }
      return {
        kind: "failed",
        error: new Error(outcome.error || "Gemini could not complete the edit."),
      };
    }

    return { kind: "failed", error: new Error("The studio returned an unknown edit status.") };
  }

  function showPendingEdit(detail) {
    state.promptPending = true;
    state.interruptPending = false;
    elements.composeButton.disabled = false;
    elements.composeButton.querySelector("span").textContent = "Interrupt";
    elements.savedState.textContent = "Waiting for Gemini";
    showEditProgress({
      phase: "queued",
      detail,
      elapsedSeconds: 0,
      timeoutSeconds: 20 * 60,
    });
  }

  async function runPendingEdit(pending, capturePlayback) {
    const {
      operationId: clientOperationId,
      prompt,
      start: selectionStart,
      end: selectionEnd,
      submittedText,
    } = pending;
    let clearSubmittedPrompt = false;
    let restorePlayback = false;
    let playbackStateCaptured = false;
    showPendingEdit(pending.acceptedJob ? "Reconnecting to the active AI edit" : "Starting the AI edit");
    try {
      let accepted = pending.acceptedJob;
      if (!accepted) {
        const editBody = new URLSearchParams({
          operation_id: clientOperationId,
          prompt,
          start: String(selectionStart),
          end: String(selectionEnd),
        });
        editBody.set("batch_parameter_tools", String(pending.batchParameterTools === true));
        editBody.set("slim_prompt", String(pending.slimPrompt === true));
        editBody.set("dynamic_tools", String(pending.dynamicTools === true));
        accepted = await acceptEdit(
          clientOperationId,
          editBody,
          () => {
            if (!capturePlayback) return;
            restorePlayback = audio.isActive;
            playbackStateCaptured = true;
            audio.stop(true);
          },
        );
        if (accepted.status !== "unavailable") {
          pending.acceptedJob = accepted;
          persistPendingEdit(pending);
        }
      }
      state.activeEditJobId = accepted.status === "unavailable" ? null : accepted.id;
      let outcome = accepted.status === "unavailable" ? accepted : await pollAcceptedEdit(accepted);
      if (outcome.status === "unavailable") {
        const recovered = await reconcileUnavailableOperation(clientOperationId);
        if (recovered) {
          if (recovered.status === "queued" || recovered.status === "running") {
            pending.acceptedJob = recovered;
            persistPendingEdit(pending);
            state.activeEditJobId = recovered.id;
            outcome = await pollAcceptedEdit(recovered);
          } else {
            outcome = recovered;
          }
        }
      }
      const result = await resolveEditOutcome(outcome);
      clearSubmittedPrompt = result.kind === "completed" || result.kind === "partial";
      if (result.kind === "completed" && result.refresh) {
        try {
          await refreshAuthoritativeProject("Edit completed; refreshing the project");
        } catch (error) {
          throw new CommittedEditSyncError(error);
        }
      }
      if (clearSubmittedPrompt && elements.promptInput.value === submittedText) {
        elements.promptInput.value = "";
      }
      if (result.kind !== "completed") throw result.error;
      showToast(result.message);
    } catch (error) {
      if (clearSubmittedPrompt && elements.promptInput.value === submittedText) {
        elements.promptInput.value = "";
      }
      showError(error, "applying a prompted edit");
      elements.savedState.textContent = state.project ? `Version ${state.project.version}` : "Offline";
    } finally {
      clearPendingEdit(clientOperationId);
      state.promptPending = false;
      state.activeEditJobId = null;
      state.interruptPending = false;
      hideEditProgress();
      elements.composeButton.disabled = false;
      elements.composeButton.querySelector("span").textContent = "Make change";
      reconcilePlaybackReadiness();
      if (playbackStateCaptured && restorePlayback && !audio.isActive) await audio.start();
    }
  }

  async function submitPrompt(event) {
    event.preventDefault();
    if (state.promptPending) {
      if (state.interruptPending || state.activeEditJobId === null) return;
      state.interruptPending = true;
      elements.composeButton.querySelector("span").textContent = "Interrupting...";
      try {
        await api(`/api/edits/${encodeURIComponent(state.activeEditJobId)}/interrupt`, {
          method: "POST",
        });
      } catch (error) {
        state.interruptPending = false;
        showError(error, "interrupting the prompted edit");
      }
      return;
    }
    if (state.promptSubmissionClaimed) return;
    const submittedText = elements.promptInput.value;
    const prompt = submittedText.trim();
    if (!prompt) return;
    state.promptSubmissionClaimed = true;
    state.promptPending = true;
    elements.composeButton.disabled = true;
    elements.composeButton.querySelector("span").textContent = "Starting...";
    let handedOff = false;
    try {
      const pending = {
        operationId: operationId(),
        prompt,
        submittedText,
        start: state.selectionStart,
        end: state.selectionEnd,
        acceptedJob: null,
        batchParameterTools: elements.batchParameterTools.checked,
        slimPrompt: elements.slimPrompt.checked,
        dynamicTools: elements.dynamicTools.checked,
      };
      persistPendingEdit(pending);
      handedOff = true;
      await runPendingEdit(pending, true);
    } catch (error) {
      if (handedOff) throw error;
      state.promptPending = false;
      elements.composeButton.disabled = false;
      elements.composeButton.querySelector("span").textContent = "Make change";
      showError(error, "preparing the prompted edit");
    } finally {
      state.promptSubmissionClaimed = false;
    }
  }

  function undo() {
    return enqueueProjectMutation(applyUndo);
  }

  async function applyUndo() {
    try {
      await replaceProject(
        async () => {
          adoptProject(await api("/api/undo", { method: "POST" }));
          renderProject();
          await loadProjectHistory(state.project.version);
        },
        { resumePlayback: false },
      );
      showToast("Last change undone");
    } catch (error) {
      showError(error, "undoing a project change");
    }
  }

  async function reset() {
    if (!window.confirm("Reset to an empty project? You can still undo this.")) return;
    await enqueueProjectMutation(applyReset);
  }

  async function applyReset() {
    try {
      await replaceProject(
        async () => {
          adoptProject(await api("/api/reset", { method: "POST" }));
          state.selectionStart = 0;
          state.selectionEnd = state.project.duration;
          renderProject();
        },
        { preservePosition: false, resumePlayback: false },
      );
      showToast("Demo arrangement restored");
    } catch (error) {
      showError(error, "resetting the project");
    }
  }

  function setView(view) {
    const views = [
      { name: "ai", button: elements.aiModeButton, panel: elements.aiModePanel },
      { name: "debug", button: elements.debugButton, panel: elements.debugPanel },
    ];
    if (!views.some((entry) => entry.name === view)) return;
    state.activeView = view;
    for (const entry of views) {
      const active = entry.name === view;
      entry.button.classList.toggle("is-active", active);
      entry.button.setAttribute("aria-selected", String(active));
      entry.button.tabIndex = active ? 0 : -1;
      entry.panel.hidden = !active;
      entry.panel.inert = !active;
    }
    if (view === "debug") renderDebug();
    if (view === "ai" && state.project) {
      renderSelection();
      renderPlayhead();
    }
    window.scrollTo(0, 0);
  }

  function openDebug() {
    setView("debug");
    void loadGeminiSessions();
  }

  function skipToTimeline(event) {
    event.preventDefault();
    setView("ai");
    elements.timelinePanel.focus({ preventScroll: true });
    elements.timelinePanel.scrollIntoView({ block: "start" });
  }

  function handleViewTabKey(event) {
    const tabs = [elements.aiModeButton, elements.debugButton];
    const current = tabs.indexOf(event.currentTarget);
    let next = current;
    if (event.key === "ArrowRight") next = (current + 1) % tabs.length;
    else if (event.key === "ArrowLeft") next = (current + tabs.length - 1) % tabs.length;
    else if (event.key === "Home") next = 0;
    else if (event.key === "End") next = tabs.length - 1;
    else return;
    event.preventDefault();
    tabs[next].click();
    tabs[next].focus();
  }

  function debugReport() {
    const project = state.project;
    const lines = [
      "DAW-AI debug report",
      `Generated: ${new Date().toISOString()}`,
      `URL: ${window.location.href}`,
      `User agent: ${navigator.userAgent}`,
      `Viewport: ${window.innerWidth}x${window.innerHeight} at ${window.devicePixelRatio || 1}x`,
      `Network: ${navigator.onLine ? "online" : "offline"}`,
      `View: ${state.activeView}`,
      `Audio: ${audio.playbackState}; continuous stream ${audio.audioVersion ?? "not loaded"}`,
      `AI edit: ${state.promptPending ? "pending" : "idle"}`,
      `AI sessions: ${state.geminiSessions.length} retained locally`,
      `Batch parameter tools: ${elements.batchParameterTools.checked ? "enabled" : "disabled"}`,
      `Slim Gemini prompt: ${elements.slimPrompt.checked ? "enabled" : "disabled"}`,
      `Dynamic tools: ${elements.dynamicTools.checked ? "enabled" : "disabled"}`,
      `Selection: ${state.selectionStart.toFixed(1)}s - ${state.selectionEnd.toFixed(1)}s`,
    ];
    if (project) {
      lines.push(
        `Project: ${project.name}`,
        `Project version: ${project.version}`,
        `Arrangement: ${project.bpm} BPM; ${project.duration}s; ${project.tracks.length} tracks; ${project.edits.length} edits`,
      );
    } else {
      lines.push("Project: unavailable");
    }
    lines.push("", "Recent AI sessions:");
    if (state.geminiSessions.length === 0) {
      lines.push("None found.");
    } else {
      for (const session of state.geminiSessions.slice(0, 10)) {
        lines.push(
          `${new Date(Number(session.createdAt) || 0).toISOString()} [${session.model || "Unknown provider"}; ${session.status || "unknown"}] ` +
            `${session.appliedSteps || 0} edit actions, ${session.audioListens || 0} listens, batch ${session.batchParameterTools ? "on" : "off"}, slim ${session.slimPrompt ? "on" : "off"}, dynamic ${session.dynamicToolLoading ? "on" : "off"}: ${session.prompt || ""}`,
          `  Metrics: ${sessionMetricsSummary(session)}`,
          `  Tool calls: ${JSON.stringify(session.metrics?.toolCalls || {})}`,
        );
      }
    }
    lines.push("", "Recent browser errors and warnings:");
    if (state.clientIssues.length === 0) {
      lines.push("None recorded in this browser session.");
    } else {
      for (const issue of state.clientIssues) {
        lines.push(`${issue.time} [${issue.level.toUpperCase()}] ${issue.context}: ${issue.message}`);
      }
    }
    lines.push("", "Backend warnings and errors are written to the DAW-AI server's stderr.");
    return lines.join("\n");
  }

  function sessionMetricsSummary(session) {
    const metrics = session.metrics || {};
    const seconds = Math.round((Number(metrics.durationMs) || 0) / 1000);
    const applyRate = `${((Number(metrics.auditionApplyRate) || 0) * 100).toFixed(0)}%`;
    return `${seconds}s, ${Number(metrics.inputTokens) || 0} input tokens, ${Number(metrics.outputTokens) || 0} output tokens, ${Number(metrics.thoughtTokens) || 0} thinking tokens, ${Number(metrics.totalToolCalls) || 0} tool calls, ${Number(metrics.failedToolCalls) || 0} failed, ${Number(metrics.mutationsBeforeFirstListen) || 0} mutations before first listen, ${Number(metrics.averageMutationsBetweenListens) || 0} average / ${Number(metrics.maxMutationsBetweenListens) || 0} max mutations between listens, ${Number(metrics.auditions) || 0} auditions, ${Number(metrics.appliedAuditions) || 0} applied (${applyRate})`;
  }

  function renderDebug() {
    elements.debugReport.value = debugReport();
    if (state.geminiSessions.length === 0) {
      elements.geminiSessionList.innerHTML = '<div class="empty-log">No AI sessions recorded yet.</div>';
      return;
    }
    elements.geminiSessionList.innerHTML = state.geminiSessions
      .slice(0, 20)
      .map(
        (session) => `<article class="gemini-session-item">
          <div><strong>${escapeHtml(new Date(Number(session.createdAt) || 0).toLocaleString())}</strong>
          <span>${escapeHtml(session.model || "Unknown provider")} &middot; ${escapeHtml(session.status || "unknown")} &middot; ${Number(session.appliedSteps) || 0} actions &middot; ${Number(session.audioListens) || 0} listens &middot; batch ${session.batchParameterTools ? "on" : "off"} &middot; slim ${session.slimPrompt ? "on" : "off"} &middot; dynamic ${session.dynamicToolLoading ? "on" : "off"}</span></div>
          <p>${escapeHtml(session.prompt || "Untitled edit")}</p>
          <p>${escapeHtml(sessionMetricsSummary(session))}</p>
        </article>`,
      )
      .join("");
  }

  async function copyDebugReport() {
    renderDebug();
    try {
      if (!navigator.clipboard?.writeText) throw new Error("Clipboard API unavailable");
      await navigator.clipboard.writeText(elements.debugReport.value);
    } catch (_error) {
      elements.debugReport.focus();
      elements.debugReport.select();
      if (!document.execCommand("copy")) {
        showToast("Select the diagnostic report and copy it manually.", true);
        return;
      }
      elements.debugReport.setSelectionRange(0, 0);
    }
    showToast("Diagnostic report copied");
  }

  function clearDebugIssues() {
    state.clientIssues = [];
    renderDebug();
    showToast("Browser issues cleared");
  }

  async function loadGeminiSessions() {
    try {
      const response = await api("/api/gemini-sessions");
      state.geminiSessions = Array.isArray(response.sessions) ? response.sessions : [];
      renderDebug();
    } catch (error) {
      showError(error, "loading AI sessions");
    }
  }

  function dismissToast() {
    window.clearTimeout(state.toastTimer);
    state.toastTimer = null;
    elements.toast.hidden = true;
  }

  function showToast(message, isError = false) {
    dismissToast();
    const dismissAfterMs = isError ? ERROR_TOAST_DISMISS_MS : TOAST_DISMISS_MS;
    elements.toastMessage.textContent = message;
    elements.toast.classList.toggle("is-error", isError);
    elements.toast.setAttribute("role", isError ? "alert" : "status");
    elements.toast.setAttribute("aria-live", isError ? "assertive" : "polite");
    elements.toast.dataset.autoDismissMs = String(dismissAfterMs);
    elements.toast.hidden = false;
    state.toastTimer = window.setTimeout(() => {
      dismissToast();
    }, dismissAfterMs);
  }

  function updateTransport() {
    elements.currentTime.textContent = formatTime(audio.playhead, true);
    elements.playButton.classList.toggle("is-playing", audio.isActive);
    elements.playButton.setAttribute("aria-label", audio.isActive ? "Pause project" : "Play project");
    document.documentElement.dataset.audioState = audio.playbackState;
  }

  function formatTime(seconds, tenths) {
    const minutes = Math.floor(seconds / 60);
    const remainder = seconds - minutes * 60;
    return `${minutes}:${Math.floor(remainder).toString().padStart(2, "0")}${tenths ? `.${Math.floor((remainder % 1) * 10)}` : ""}`;
  }

  function clamp(value, minimum, maximum) {
    return Math.min(maximum, Math.max(minimum, value));
  }

  function quantize(value, amount) {
    return Math.round(value / amount) * amount;
  }

  function escapeHtml(value) {
    return String(value)
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;")
      .replaceAll("'", "&#039;");
  }

  elements.trackRows.addEventListener("pointerdown", beginSelection);
  elements.trackRows.addEventListener("pointermove", moveSelection);
  elements.trackRows.addEventListener("pointerup", endSelection);
  elements.trackRows.addEventListener("pointercancel", endSelection);
  elements.trackRows.addEventListener("dblclick", selectWholeTrackFromDoubleClick);
  elements.trackRows.addEventListener("contextmenu", keepLongPressForTimeline);
  elements.trackRows.addEventListener("keydown", handleTimelineKey);
  elements.promptForm.addEventListener("submit", submitPrompt);
  elements.playButton.addEventListener("click", () => void audio.toggle());
  elements.rewindButton.addEventListener("click", () => audio.seek(0));
  elements.undoButton.addEventListener("click", () => void undo());
  elements.aiDurationButton.addEventListener("click", () => void editDuration());
  elements.resetButton.addEventListener("click", () => void reset());
  elements.selectionModeButton.addEventListener("click", () => setTouchSelectionMode(!state.touchSelectionMode));
  elements.skipLink.addEventListener("click", skipToTimeline);
  elements.aiModeButton.addEventListener("click", () => setView("ai"));
  elements.debugButton.addEventListener("click", openDebug);
  [elements.aiModeButton, elements.debugButton].forEach((button) => {
    button.addEventListener("keydown", handleViewTabKey);
  });
  elements.copyDebug.addEventListener("click", () => void copyDebugReport());
  elements.clearDebug.addEventListener("click", clearDebugIssues);
  elements.refreshGeminiSessions.addEventListener("click", () => void loadGeminiSessions());
  elements.batchParameterTools.addEventListener("change", () => {
    try {
      window.localStorage.setItem(
        BATCH_PARAMETER_TOOLS_STORAGE_KEY,
        String(elements.batchParameterTools.checked),
      );
    } catch (_error) {
      // An unavailable preference store must not prevent the experiment.
    }
    renderDebug();
  });
  elements.slimPrompt.addEventListener("change", () => {
    try {
      window.localStorage.setItem(SLIM_PROMPT_STORAGE_KEY, String(elements.slimPrompt.checked));
    } catch (_error) {
      // An unavailable preference store must not prevent the experiment.
    }
    renderDebug();
  });
  elements.dynamicTools.addEventListener("change", () => {
    try {
      window.localStorage.setItem(DYNAMIC_TOOLS_STORAGE_KEY, String(elements.dynamicTools.checked));
    } catch (_error) {
      // An unavailable preference store must not prevent the experiment.
    }
    renderDebug();
  });
  elements.sessionHistoryList.addEventListener("click", (event) => {
    void enqueueProjectMutation(() => selectProjectHistory(event));
  });
  elements.toastClose.addEventListener("click", dismissToast);
  document.querySelectorAll("[data-prompt]").forEach((button) => {
    button.addEventListener("click", () => {
      elements.promptInput.value = button.dataset.prompt;
      elements.promptInput.focus();
    });
  });
  elements.promptInput.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      if (!state.promptPending) elements.promptForm.requestSubmit();
    }
  });
  window.addEventListener("error", (event) => {
    reportClientIssue("error", event.error || event.message, "uncaught browser error");
  });
  window.addEventListener("unhandledrejection", (event) => {
    reportClientIssue("error", event.reason, "unhandled browser promise rejection");
  });
  window.addEventListener("resize", () => {
    renderSelection();
    renderPlayhead();
    renderDebug();
  });
  document.addEventListener("keydown", (event) => {
    const nativeSpaceSelector = "textarea, input, button, select, summary, a[href], [contenteditable='true']";
    const nativeSpaceControl =
      event.target.closest?.(nativeSpaceSelector) ?? document.activeElement?.closest?.(nativeSpaceSelector);
    if (event.code === "Space" && !nativeSpaceControl) {
      event.preventDefault();
      void audio.toggle();
    }
  });

  async function initialize() {
    try {
      elements.batchParameterTools.checked =
        window.localStorage.getItem(BATCH_PARAMETER_TOOLS_STORAGE_KEY) === "true";
    } catch (_error) {
      elements.batchParameterTools.checked = false;
    }
    try {
      elements.slimPrompt.checked =
        window.localStorage.getItem(SLIM_PROMPT_STORAGE_KEY) === "true";
    } catch (_error) {
      elements.slimPrompt.checked = false;
    }
    try {
      elements.dynamicTools.checked =
        window.localStorage.getItem(DYNAMIC_TOOLS_STORAGE_KEY) === "true";
    } catch (_error) {
      elements.dynamicTools.checked = false;
    }
    const pending = readPendingEdit();
    if (pending) {
      if (!elements.promptInput.value) elements.promptInput.value = pending.submittedText;
      showPendingEdit("Reconnecting to the active AI edit");
    }
    try {
      await audio.initialize();
    } catch (error) {
      showError(error, "initializing audio", "Could not initialize audio: ");
    }
    await loadProject();
    await loadGeminiSessions();
    if (pending && state.project) await runPendingEdit(pending, false);
  }

  void initialize();
})();
