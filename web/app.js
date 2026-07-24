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
    resetButton: document.querySelector("#reset-button"),
    savedState: document.querySelector("#saved-state"),
    historyCount: document.querySelector("#history-count"),
    aiModeButton: document.querySelector("#ai-mode-button"),
    aiModePanel: document.querySelector("#ai-mode-panel"),
    advancedButton: document.querySelector("#advanced-button"),
    closeAdvanced: document.querySelector("#close-advanced"),
    advancedDrawer: document.querySelector("#advanced-drawer"),
    channelCreator: document.querySelector("#channel-creator"),
    channelRole: document.querySelector("#channel-role"),
    addChannel: document.querySelector("#add-channel"),
    channelList: document.querySelector("#channel-list"),
    debugButton: document.querySelector("#debug-button"),
    debugPanel: document.querySelector("#debug-panel"),
    debugReport: document.querySelector("#debug-report"),
    copyDebug: document.querySelector("#copy-debug"),
    clearDebug: document.querySelector("#clear-debug"),
    refreshGeminiSessions: document.querySelector("#refresh-gemini-sessions"),
    geminiSessionList: document.querySelector("#gemini-session-list"),
    audioBackend: document.querySelector("#audio-backend"),
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
    activeEditJobId: null,
    interruptPending: false,
    editProgressPercent: 0,
    channelMutationPending: false,
    centeredInitialSelection: false,
    toastTimer: null,
    activeView: "ai",
    clientIssues: [],
    geminiSessions: [],
    projectHistory: { current: 0, entries: [] },
    graphNodeSelection: {},
    midiEventSelection: {},
    instrumentParameters: {},
  };
  let historyLoadQueue = Promise.resolve();

  let projectMutationQueue = Promise.resolve();
  const RECONCILED_REQUEST_TIMEOUT_MS = 2000;
  const EDIT_ACCEPTANCE_TIMEOUT_MS = 10_000;
  const PENDING_EDIT_STORAGE_KEY = "daw-ai.pending-edit.v1";
  const AUDIO_RETRY_DELAYS_MS = [250, 500, 1000];
  const AUDIO_SEEK_DEBOUNCE_MS = 200;
  const TOAST_DISMISS_MS = 4200;
  const ERROR_TOAST_DISMISS_MS = 60_000;
  const LONG_PRESS_MS = 500;
  const LONG_PRESS_MOVE_TOLERANCE_PX = 10;
  const SURGE_PRESETS = [
    "Init",
    "Surge Kick",
    "Surge Snare",
    "Surge Closed Hat",
    "Surge Open Hat",
    "Surge Crash",
    "Surge Percussion",
    "Surge Bass",
    "Surge Pad",
    "Surge Lead",
    "Surge Atmosphere",
  ];

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
      this.media.addEventListener("ended", () => {
        if (this.isActive) this.stop(false);
      });
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
      if (this.isActive) {
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
      return Promise.resolve(playback)
        .then(() => {
          if (
            generation !== this.playbackGeneration ||
            streamAttempt !== this.streamAttempt ||
            !this.isActive
          ) {
            return;
          }
          this.playbackState = "playing";
          window.clearInterval(this.timer);
          this.timer = window.setInterval(() => this.tick(), 50);
          this.tick();
          updateTransport();
        })
        .catch((error) => {
          this.retryPlayback(error, generation, streamAttempt);
        });
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
      this.audioUrl = null;
      this.audioVersion = null;
      if (!preservePosition) this.playhead = 0;
      updateTransport();
      renderPlayhead();
    }

    seek(time) {
      const wasActive = this.isActive;
      if (wasActive) this.stop(true);
      this.playhead = clamp(time, 0, this.project?.duration ?? 0);
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
          void this.startStream(generation);
        }
      }, delay);
      updateTransport();
    }

    tick() {
      if (!this.isPlaying) return;
      this.updatePosition();
      if (this.retryAttempts > 0 && this.media.currentTime >= 2) this.retryAttempts = 0;
      if (this.playhead >= this.project.duration) {
        this.stop(false);
        return;
      }
      updateTransport();
      renderPlayhead();
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

  function adoptProject(project) {
    const previousProject = state.project;
    for (const [key, parameters] of Object.entries(state.instrumentParameters)) {
      const trackId = Number(key.split(":", 1)[0]);
      const previousTrack = previousProject?.tracks.find((track) => track.id === trackId);
      const track = project?.tracks.find((candidate) => candidate.id === trackId);
      if (
        !previousTrack ||
        !track ||
        previousTrack.instrument.id !== track.instrument.id ||
        previousTrack.instrument.preset !== track.instrument.preset
      ) {
        delete state.instrumentParameters[key];
        continue;
      }
      const overrides = track.instrument.nativeOverrides || {};
      for (const parameter of parameters) {
        const id = parameter.parameter?.startsWith("native:")
          ? parameter.parameter.slice("native:".length)
          : null;
        if (id == null) continue;
        const overridden = Object.prototype.hasOwnProperty.call(overrides, id);
        parameter.value = overridden ? Number(overrides[id]) : parameter.presetValue;
        parameter.overridden = overridden;
      }
    }
    state.project = project;
    return project;
  }

  async function loadProject() {
    try {
      adoptProject(await api("/api/project"));
      const backend = await api("/api/backend");
      elements.audioBackend.value = backend.backend;
      renderProject();
    } catch (error) {
      showError(error, "loading the project");
      elements.savedState.textContent = "Offline";
    }
  }

  async function changeAudioBackend() {
    try {
      const response = await api("/api/backend", {
        method: "POST",
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ backend: elements.audioBackend.value }),
      });
      elements.audioBackend.value = response.backend;
      audio.stop(true);
      showToast(`Using ${response.backend} instrument backend`);
    } catch (error) {
      showError(error, "changing the instrument backend");
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
    renderSelection();
    renderPlayhead();
    renderAdvanced();
    renderDebug();
    updateTransport();
    if (!state.centeredInitialSelection) {
      state.centeredInitialSelection = true;
      window.requestAnimationFrame(centerSelectionOnNarrowTimeline);
    }
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
        const audioClips = (track.audioClips || [])
          .map((clip) => {
            const left = (clip.start / duration) * 100;
            const width = ((clip.end - clip.start) / duration) * 100;
            return `<div class="clip audio-clip ${track.muted ? "is-muted" : ""}" style="left:${left}%;width:${width}%;--track-color:${track.color}">
              <span class="clip-name">${escapeHtml(clip.label)}${clip.reversed ? " (reversed)" : ""}</span>
              <span class="audio-waveform" aria-hidden="true">${Array.from({ length: 24 }, (_, index) => `<i style="--wave-height:${25 + ((index * 37 + clip.id) % 70)}%"></i>`).join("")}</span>
            </div>`;
          })
          .join("");
        const clips = `${midiClips}${audioClips}`;
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

  function renderAdvanced() {
    const uiState = captureAdvancedUiState();
    elements.channelList.innerHTML = state.project.tracks
      .map((track) => {
        const selectedNode = selectedGraphNode(track);
        const regionalEffects = regionalEffectsForTrack(track)
          .map((effect) => {
            return `<span class="effect-pill is-regional">${escapeHtml(effect.name)} <b>${escapeHtml(effect.detail)} &middot; ${effect.start.toFixed(1)}-${effect.end.toFixed(1)}s</b></span>`;
          })
          .join("");
        const orderedEffects = track.routing.audio
          .filter((node) => node.startsWith("effect:"))
          .map((node) => track.effects.find((effect) => effect.id === Number(node.slice(7))))
          .filter(Boolean);
        return `<section class="channel-card" data-channel-track="${track.id}" tabindex="-1" style="--track-color:${track.color}">
          <div class="channel-heading">
            <div class="channel-name"><i></i>${escapeHtml(track.name)}</div>
            <div class="channel-actions">
              <button class="mute-button ${track.muted ? "is-muted" : ""}" type="button" data-mute-track="${track.id}" data-muted="${track.muted}">${track.muted ? "MUTED" : "MUTE"}</button>
              <button class="delete-channel-button" type="button" data-delete-track="${track.id}" data-track-name="${escapeHtml(track.name)}" aria-label="${escapeHtml(`Delete ${track.name} track`)}" ${state.channelMutationPending ? "disabled" : ""}>Delete</button>
            </div>
          </div>
          <label class="volume-control">LEVEL
            <input type="range" min="0" max="1.5" step="0.01" value="${track.volume}" data-volume-track="${track.id}" aria-label="${escapeHtml(track.name)} volume">
            <output>${Math.round(track.volume * 100)}%</output>
          </label>
          <div class="track-workspace">
            <div class="track-editor-column">
              <div class="sound-graph-panel">
                <div class="tool-heading"><div><span>Sound graph</span><strong>Click a node to inspect it</strong></div></div>
                ${renderSoundGraph(track, orderedEffects, selectedNode)}
              </div>
              <div class="sound-tool clips-tool">
                <div class="tool-heading"><div><span>MIDI Clips</span><strong>Piano roll and event editor</strong></div></div>
                ${track.clips.map((clip) => renderClipTimeline(track, clip)).join("")}
                ${(track.audioClips || []).map((clip) => renderAudioClipTimeline(track, clip)).join("")}
                ${track.clips.length === 0 && (track.audioClips || []).length === 0 ? '<span class="effect-pill">No clips</span>' : ""}
              </div>
            </div>
            <aside class="node-inspector-column" aria-label="${escapeHtml(`${track.name} selected node parameters`)}">
              <div class="inspector-label">Selected node</div>
              <div class="sound-tool instrument-tool node-inspector" data-node-pane="instrument:${track.instrument.id}" ${selectedNode === `instrument:${track.instrument.id}` ? "" : "hidden"}>
                <div class="tool-heading"><div><span>Instrument</span><strong>${escapeHtml(track.instrument.engine)}</strong></div><code>#${track.instrument.id}</code></div>
                <div class="tool-controls instrument-preset-controls">
                  <label class="tool-control">Preset
                    <select data-sound-tool="instrument" data-track-id="${track.id}" data-tool-id="${track.instrument.id}" data-parameter="preset" data-control-key="${track.id}-instrument-${track.instrument.id}-preset" aria-label="${escapeHtml(`${track.name} instrument #${track.instrument.id} Surge XT preset`)}">
                      ${selectOptions(SURGE_PRESETS.includes(track.instrument.preset) ? SURGE_PRESETS : [track.instrument.preset, ...SURGE_PRESETS], track.instrument.preset)}
                    </select>
                  </label>
                </div>
                <h4>Common controls</h4>
                <div class="tool-controls instrument-envelope-controls">
                  ${(state.instrumentParameters[`${track.id}:common`] || []).map((parameter) =>
                    soundRange(track, "instrument", track.instrument.id, "instrument", parameter.parameter, parameter.value, 0, 1, "%", "", `${parameter.name}${parameter.overridden ? " (overridden)" : ` - preset: ${parameter.display}`}`)
                  ).join("")}
                  ${state.instrumentParameters[`${track.id}:common`] ? "" : `<button type="button" data-load-instrument-parameters="${track.id}:common">Show common controls</button>`}
                </div>
                <details class="instrument-advanced-controls">
                  <summary>Advanced controls</summary>
                  <div class="tool-controls">
                    ${(state.instrumentParameters[`${track.id}:advanced`] || []).map((parameter) =>
                      soundRange(track, "instrument", track.instrument.id, "instrument", parameter.parameter, parameter.value, 0, 1, "%", "", `${parameter.name}${parameter.overridden ? " (overridden)" : ` - preset: ${parameter.display}`}`)
                    ).join("")}
                    ${state.instrumentParameters[`${track.id}:advanced`] ? "" : `<button type="button" data-load-instrument-parameters="${track.id}:advanced">Show advanced controls</button>`}
                  </div>
                </details>
              </div>
              <div class="sound-tool effects-tool node-inspector" data-node-pane="effects" ${selectedNode.startsWith("effect:") ? "" : "hidden"}>
                <div class="tool-heading"><div><span>Effect</span><strong>Audio processor parameters</strong></div></div>
                <div class="effect-stack">${orderedEffects.map((effect, index) => `<div data-effect-pane="effect:${effect.id}" ${selectedNode === `effect:${effect.id}` ? "" : "hidden"}>${renderEffect(track, effect, index, orderedEffects.length)}</div>`).join("")}</div>
                <div class="effects-list">${regionalEffects || '<span class="effect-pill">No regional effects</span>'}</div>
              </div>
              <div class="sound-tool modulators-tool node-inspector" data-node-pane="modulators" ${selectedNode.startsWith("modulator:") ? "" : "hidden"}>
                <div class="tool-heading"><div><span>Modulator</span><strong>Control signal parameters</strong></div></div>
                ${track.modulators.map((modulator) => `<div data-modulator-pane="modulator:${modulator.id}" ${selectedNode === `modulator:${modulator.id}` ? "" : "hidden"}>${renderModulator(track, modulator)}</div>`).join("")}
              </div>
            </aside>
          </div>
        </section>`;
      })
      .join("");
    restoreAdvancedUiState(uiState);

    elements.channelList.querySelectorAll("[data-volume-track]").forEach((input) => {
      input.addEventListener("input", () => {
        input.nextElementSibling.value = `${Math.round(Number(input.value) * 100)}%`;
      });
      input.addEventListener("change", () => {
        void changeMix(input.dataset.volumeTrack, { volume: input.value }, "volume");
      });
    });
    elements.channelList.querySelectorAll("[data-mute-track]").forEach((button) => {
      button.addEventListener("click", () => {
        void changeMix(button.dataset.muteTrack, { muted: String(button.dataset.muted !== "true") }, "mute");
      });
    });
    elements.channelList.querySelectorAll("[data-delete-track]").forEach((button) => {
      button.addEventListener("click", () => {
        if (!window.confirm(`Delete the ${button.dataset.trackName} track and all of its sound tools?`)) return;
        void changeChannel("delete", { track_id: button.dataset.deleteTrack });
      });
    });
    elements.channelList.querySelectorAll("[data-load-instrument-parameters]").forEach((button) => {
      button.addEventListener("click", async () => {
        const [trackId, group] = button.dataset.loadInstrumentParameters.split(":");
        button.disabled = true;
        try {
          const response = await api(`/api/instrument-parameters/${trackId}/${group}`);
          state.instrumentParameters[`${trackId}:${group}`] = response.parameters;
          renderAdvanced();
        } catch (error) {
          button.disabled = false;
          showError(error, `loading ${group} instrument controls`);
        }
      });
    });
    elements.channelList.querySelectorAll("[data-graph-node]").forEach((button) => {
      button.addEventListener("click", () => {
        state.graphNodeSelection[button.dataset.trackId] = button.dataset.graphNode;
        const trackId = button.dataset.trackId;
        const node = button.dataset.graphNode;
        renderAdvanced();
        [...elements.channelList.querySelectorAll("[data-graph-node]")]
          .find((candidate) => candidate.dataset.trackId === trackId && candidate.dataset.graphNode === node)
          ?.focus({ preventScroll: true });
      });
    });
    elements.channelList.querySelectorAll("[data-midi-event]").forEach((note) => {
      note.addEventListener("click", () => {
        const clipKey = note.closest("[data-clip-key]").dataset.clipKey;
        state.midiEventSelection[clipKey] = Number(note.dataset.midiEvent);
        renderAdvanced();
        elements.channelList
          .querySelector(`[data-clip-key="${clipKey}"] [data-tool-id="${note.dataset.midiEvent}"][data-parameter="pitch"]`)
          ?.focus({ preventScroll: true });
      });
    });
    elements.channelList.querySelectorAll("[data-sound-tool]").forEach((control) => {
      validateSoundToolControl(control);
      if (control.matches('input[type="range"]')) {
        control.addEventListener("input", () => {
          validateSoundToolControl(control);
          const output = control.nextElementSibling;
          if (output?.matches("output")) output.value = formatSoundValue(control.value, control.dataset.unit);
        });
      }
      control.addEventListener("change", () => {
        validateSoundToolControl(control);
        void changeSoundTool(control, control.value);
      });
    });
    elements.channelList.querySelectorAll("[data-sound-value]").forEach((button) => {
      button.addEventListener("click", () => {
        void changeSoundTool(button, button.dataset.soundValue);
      });
    });
  }

  function selectedGraphNode(track) {
    const available = [
      `instrument:${track.instrument.id}`,
      ...track.effects.map((effect) => `effect:${effect.id}`),
      ...track.modulators.map((modulator) => `modulator:${modulator.id}`),
    ];
    const selected = state.graphNodeSelection[track.id];
    if (available.includes(selected)) return selected;
    state.graphNodeSelection[track.id] = available[0];
    return available[0];
  }

  function renderSoundGraph(track, orderedEffects, selectedNode) {
    const routeNodes = [
      { label: "MIDI Clips", type: "clip" },
      { label: track.instrument.engine, type: "instrument", key: `instrument:${track.instrument.id}` },
      ...orderedEffects.map((effect) => ({ label: effect.name, type: "effect", key: `effect:${effect.id}` })),
      { label: "Master", type: "output" },
    ];
    const route = routeNodes
      .map((node, index) => {
        const signal = index === 0 ? "MIDI" : "AUDIO";
        const edge = index < routeNodes.length - 1
          ? `<i aria-hidden="true"><b>${signal}</b>&rarr;</i>`
          : "";
        const content = node.key
          ? `<button type="button" class="graph-node graph-node-${node.type} ${selectedNode === node.key ? "is-selected" : ""}" data-graph-node="${node.key}" data-track-id="${track.id}" aria-pressed="${selectedNode === node.key}"><span>${escapeHtml(node.label)}</span><small>${escapeHtml(node.type)}</small></button>`
          : `<span class="graph-terminal graph-node-${node.type}">${escapeHtml(node.label)}</span>`;
        return `${content}${edge}`;
      })
      .join("");
    const modulators = track.modulators
      .map((modulator) => `<button type="button" class="graph-node graph-node-modulator ${selectedNode === `modulator:${modulator.id}` ? "is-selected" : ""}" data-graph-node="modulator:${modulator.id}" data-track-id="${track.id}" aria-pressed="${selectedNode === `modulator:${modulator.id}`}"><span>${escapeHtml(modulator.name)}</span><small>CONTROL &rarr; ${escapeHtml(modulator.target)}</small></button>`)
      .join("");
    return `<div class="sound-graph" role="group" aria-label="${escapeHtml(`${track.name} sound graph`)}">
      <div class="routing-chain" aria-label="${escapeHtml(`${track.name} typed sound routing`)}">${route}</div>
      <div class="graph-control-nodes">${modulators}</div>
    </div>`;
  }

  function captureAdvancedUiState() {
    const clips = new Map();
    for (const editor of elements.channelList.querySelectorAll("[data-clip-key]")) {
      clips.set(editor.dataset.clipKey, { open: editor.open });
    }
    return { drawerScrollTop: elements.advancedDrawer.scrollTop, clips };
  }

  function restoreAdvancedUiState(uiState) {
    elements.advancedDrawer.scrollTop = uiState.drawerScrollTop;
    for (const editor of elements.channelList.querySelectorAll("[data-clip-key]")) {
      const clipState = uiState.clips.get(editor.dataset.clipKey);
      if (!clipState) continue;
      editor.open = clipState.open;
    }
  }

  function soundRange(
    track,
    tool,
    toolId,
    ownerName,
    parameter,
    value,
    minimum,
    maximum,
    unit,
    clipId = "",
    label = parameter,
  ) {
    const key = `${track.id}-${tool}-${toolId}-${parameter}`;
    const clipAttribute = clipId === "" ? "" : ` data-clip-id="${clipId}"`;
    const owner = tool === "instrument" ? "instrument" : `${ownerName} ${tool}`;
    const accessibleName = `${track.name} ${owner} #${toolId} ${parameter}`;
    return `<label class="tool-control">${escapeHtml(label)}
      <span class="range-with-output"><input type="range" min="${minimum}" max="${maximum}" step="any" value="${value}" data-sound-tool="${tool}" data-track-id="${track.id}" data-tool-id="${toolId}" data-parameter="${parameter}" data-unit="${unit}" data-control-key="${key}"${clipAttribute} aria-label="${escapeHtml(accessibleName)}"><output>${formatSoundValue(value, unit)}</output></span>
    </label>`;
  }

  function soundToggle(track, tool, toolId, name, enabled) {
    const action = enabled ? "Disable" : "Enable";
    const accessibleName = `${action} ${track.name} ${name} ${tool} #${toolId}`;
    return `<button type="button" aria-label="${escapeHtml(accessibleName)}" aria-pressed="${String(enabled)}" data-sound-tool="${tool}" data-track-id="${track.id}" data-tool-id="${toolId}" data-parameter="enabled" data-sound-value="${String(!enabled)}" data-control-key="${track.id}-${tool}-${toolId}-enabled">${enabled ? "On" : "Off"}</button>`;
  }

  function renderEffect(track, effect, index, effectCount) {
    const filterControls = Number.isFinite(effect.parameters.cutoff)
      ? [
          soundRange(
            track,
            "effect",
            effect.id,
            effect.name,
            "cutoff",
            effect.parameters.cutoff,
            80,
            16000,
            "Hz",
            "",
            "Cutoff",
          ),
          soundRange(
            track,
            "effect",
            effect.id,
            effect.name,
            "resonance",
            effect.parameters.resonance,
            0.1,
            20,
            "Q",
            "",
            "Resonance",
          ),
        ].join("")
      : "";
    const detailedControls = Object.entries(effect.parameters)
      .filter(([parameter, value]) => !["mix", "cutoff", "resonance"].includes(parameter) && Number.isFinite(value))
      .map(([parameter, value]) => soundRange(
        track,
        "effect",
        effect.id,
        effect.name,
        parameter,
        value,
        0,
        1,
        "%",
        "",
        parameter.replace(/([A-Z])/g, " $1").replace(/^./, (letter) => letter.toUpperCase()),
      ))
      .join("");
    return `<div class="effect-card ${effect.enabled ? "" : "is-disabled"}">
      <div class="effect-card-heading"><span class="effect-pill"><strong>${escapeHtml(effect.name)}</strong> <b>${formatSoundValue(effect.parameters.mix, "%")}</b></span><code>#${effect.id}</code></div>
      ${soundRange(track, "effect", effect.id, effect.name, "mix", effect.parameters.mix, 0, 1, "%")}
      ${filterControls}
      ${detailedControls}
      <div class="tool-actions">
        ${soundToggle(track, "effect", effect.id, effect.name, effect.enabled)}
        <button type="button" aria-label="${escapeHtml(`Move ${track.name} ${effect.name} effect #${effect.id} earlier`)}" ${index === 0 ? "disabled" : ""} data-sound-tool="routing" data-track-id="${track.id}" data-tool-id="${effect.id}" data-parameter="position" data-sound-value="${Math.max(0, index - 1)}" data-control-key="${track.id}-routing-${effect.id}-up">&uarr;</button>
        <button type="button" aria-label="${escapeHtml(`Move ${track.name} ${effect.name} effect #${effect.id} later`)}" ${index === effectCount - 1 ? "disabled" : ""} data-sound-tool="routing" data-track-id="${track.id}" data-tool-id="${effect.id}" data-parameter="position" data-sound-value="${Math.min(effectCount - 1, index + 1)}" data-control-key="${track.id}-routing-${effect.id}-down">&darr;</button>
      </div>
    </div>`;
  }

  function renderModulator(track, modulator) {
    const nativeTargets = ["common", "advanced"]
      .flatMap((group) => state.instrumentParameters[`${track.id}:${group}`] || [])
      .map((parameter) => [parameter.parameter, `Surge: ${parameter.name}`]);
    const targets = [...track.modulationTargets.map((target) => [target.id, target.label]), ...nativeTargets];
    if (!targets.some(([value]) => value === modulator.target)) {
      targets.push([modulator.target, modulator.target]);
    }
    const sourceTrackId = modulator.sourceTrackId ?? track.id;
    const sourceTracks = state.project.tracks
      .map((source) => `<option value="${source.id}" ${source.id === sourceTrackId ? "selected" : ""}>${escapeHtml(source.name)} (#${source.id})</option>`)
      .join("");
    return `<div class="modulator-card ${modulator.enabled ? "" : "is-disabled"}">
      <div class="effect-card-heading"><strong>${escapeHtml(modulator.name)}</strong><code>#${modulator.id}</code></div>
      ${modulator.enabled && modulator.trigger !== "free" ? `<div class="modulator-route"><b>${escapeHtml(state.project.tracks.find((source) => source.id === sourceTrackId)?.name ?? "Unknown source")}</b><i aria-hidden="true">${modulator.trigger.toUpperCase()} &rarr;</i><b>Modulator</b></div>` : ""}
      <div class="tool-controls">
        <label class="tool-control">Shape
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="shape" data-control-key="${track.id}-modulator-${modulator.id}-shape" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} shape`)}">${selectOptions(["sine", "triangle", "square", "random", "envelope", "formula"], modulator.shape)}</select>
        </label>
        <label class="tool-control">Target
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="target" data-control-key="${track.id}-modulator-${modulator.id}-target" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} target`)}">${targets.map(([value, label]) => `<option value="${value}" ${value === modulator.target ? "selected" : ""}>${escapeHtml(label)}</option>`).join("")}</select>
        </label>
        <label class="tool-control">Rate mode
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="rateMode" data-control-key="${track.id}-modulator-${modulator.id}-rateMode" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} rate mode`)}">${selectOptions(["hz", "tempo"], modulator.rateMode)}</select>
        </label>
        <label class="tool-control">Trigger
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="trigger" data-control-key="${track.id}-modulator-${modulator.id}-trigger" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} trigger`)}">${selectOptions(["free", "midi", "audio"], modulator.trigger)}</select>
        </label>
        ${modulator.trigger !== "free" ? `<label>Source track
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="sourceTrackId" data-control-key="${track.id}-modulator-${modulator.id}-sourceTrackId" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} source track`)}">${sourceTracks}</select>
        </label>` : ""}
        ${modulator.trigger === "audio" ? `<label>Polarity
          <select data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="polarity" data-control-key="${track.id}-modulator-${modulator.id}-polarity" aria-label="${escapeHtml(`${track.name} ${modulator.name} modulator #${modulator.id} polarity`)}">${selectOptions(["increase", "decrease"], modulator.polarity)}</select>
        </label>` : ""}
        ${soundRange(track, "modulator", modulator.id, modulator.name, "rate", modulator.parameters.rate, 0.01, 20, modulator.rateMode === "tempo" ? "x/beat" : "Hz")}
        ${soundRange(track, "modulator", modulator.id, modulator.name, "depth", modulator.parameters.depth, 0, 1, "%")}
        <label class="tool-control">Surge Formula (Lua)
          <textarea data-sound-tool="modulator" data-track-id="${track.id}" data-tool-id="${modulator.id}" data-parameter="formula" data-control-key="${track.id}-modulator-${modulator.id}-formula" aria-label="${escapeHtml(`${track.name} ${modulator.name} formula`)}">${escapeHtml(modulator.formula || "")}</textarea>
        </label>
        ${modulator.trigger === "audio" ? `${soundRange(track, "modulator", modulator.id, modulator.name, "threshold", modulator.parameters.threshold, 0, 1, "")}${soundRange(track, "modulator", modulator.id, modulator.name, "attackMs", modulator.parameters.attackMs, 0, 1000, "ms")}${soundRange(track, "modulator", modulator.id, modulator.name, "releaseMs", modulator.parameters.releaseMs, 1, 5000, "ms")}` : ""}
      </div>
      <div class="tool-actions">${soundToggle(track, "modulator", modulator.id, modulator.name, modulator.enabled)}</div>
    </div>`;
  }

  function renderClipTimeline(track, clip) {
    const playback = clip.playback?.mode === "once"
      ? `${clip.playback.lengthBeats} beat phrase`
      : `${clip.playback?.lengthBeats ?? clip.loopBeats} beat loop`;
    const clipKey = `${track.id}-${clip.id}`;
    const selectedId = state.midiEventSelection[clipKey];
    const selected = clip.events.find((event) => event.id === selectedId);
    return `<details class="clip-editor" data-clip-key="${clipKey}" open><summary><span>${escapeHtml(clip.label)}</span><b>${clip.events.length} events &middot; ${playback}</b></summary>${renderPianoRoll(track, clip, selectedId)}${selected ? renderMidiNoteInspector(track, clip, selected) : ""}</details>`;
  }

  function renderAudioClipTimeline(track, clip) {
    const flags = [clip.reversed ? "reversed" : null, `${clip.gain}x gain`].filter(Boolean).join(" / ");
    return `<div class="clip-editor audio-clip-editor"><div class="audio-clip-summary"><span>${escapeHtml(clip.label)}</span><b>${clip.sourceDuration.toFixed(2)}s / ${flags}</b></div><div class="advanced-audio-waveform" aria-label="${escapeHtml(`${track.name} ${clip.label} audio clip`)}">${Array.from({ length: 64 }, (_, index) => `<i style="--wave-height:${20 + ((index * 43 + clip.id) % 78)}%"></i>`).join("")}</div></div>`;
  }

  function renderPianoRoll(track, clip, selectedId) {
    if (clip.events.length === 0) return '<div class="empty-piano-roll">No notes in this clip</div>';
    const pitches = clip.events.map((event) => event.pitch);
    const minimum = Math.max(0, Math.floor(Math.min(...pitches) / 12) * 12);
    const maximum = Math.min(127, Math.max(minimum + 11, Math.ceil((Math.max(...pitches) + 1) / 12) * 12 - 1));
    const pitchCount = maximum - minimum + 1;
    const rowHeight = 100 / pitchCount;
    const playbackBeats = clip.playback?.lengthBeats ?? clip.loopBeats;
    const beatWidth = 100 / playbackBeats;
    const keys = Array.from({ length: pitchCount }, (_, index) => maximum - index)
      .map((pitch) => {
        const pitchClass = pitch % 12;
        const isBlack = [1, 3, 6, 8, 10].includes(pitchClass);
        const label = pitchClass === 0 ? midiNoteName(pitch) : "";
        return `<span class="piano-key ${isBlack ? "is-black" : ""}">${label}</span>`;
      })
      .join("");
    const notes = clip.events
      .map((event) => {
        const left = (event.time / playbackBeats) * 100;
        const width = Math.max(0.8, (event.duration / playbackBeats) * 100);
        const top = (maximum - event.pitch) * rowHeight;
        return `<button type="button" class="midi-note ${event.id === selectedId ? "is-selected" : ""}" data-midi-event="${event.id}" aria-pressed="${String(event.id === selectedId)}" style="--note-left:${left}%;--note-width:${width}%;--note-top:${top}%;--note-height:${rowHeight}%;--note-velocity:${event.velocity}" aria-label="${escapeHtml(`Edit ${midiNoteName(event.pitch)} at beat ${event.time}, length ${event.duration}, velocity ${event.velocity}`)}"><span>${escapeHtml(midiNoteName(event.pitch))}</span></button>`;
      })
      .join("");
    return `<div class="piano-roll" role="group" aria-label="${escapeHtml(`${track.name} ${clip.label} piano roll`)}">
      <div class="piano-keyboard" aria-hidden="true">${keys}</div>
      <div class="piano-grid" style="--pitch-row-height:${rowHeight}%;--beat-width:${beatWidth}%">${notes}</div>
    </div>`;
  }

  function renderMidiNoteInspector(track, clip, event) {
    const key = `${track.id}-clip-${clip.id}-event-${event.id}`;
    const name = `${track.name} ${clip.label} note #${event.id}`;
    const input = (label, parameter, value, minimum, maximum, step, exclusive = false) =>
      `<label>${label}<input type="number" min="${minimum}" max="${maximum}" step="${step}" value="${value}" ${exclusive ? `data-maximum-exclusive="${maximum}"` : ""} data-sound-tool="event" data-track-id="${track.id}" data-tool-id="${event.id}" data-clip-id="${clip.id}" data-parameter="${parameter}" data-control-key="${key}-${parameter}" aria-label="${escapeHtml(`${name} ${label.toLowerCase()}`)}"></label>`;
    return `<div class="midi-note-inspector" aria-label="${escapeHtml(`${name} editor`)}">
      <strong>${escapeHtml(midiNoteName(event.pitch))}</strong>
      ${input("Beat", "time", event.time, 0, clip.playback?.lengthBeats ?? clip.loopBeats, "any", true)}
      ${input("Length", "duration", event.duration, 0.0625, clip.playback?.lengthBeats ?? clip.loopBeats, "any")}
      ${input("Pitch", "pitch", event.pitch, 0, 127, 1)}
      ${input("Velocity", "velocity", event.velocity, 0.01, 1, "any")}
    </div>`;
  }

  function midiNoteName(pitch) {
    const names = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    return `${names[pitch % 12]}${Math.floor(pitch / 12) - 1}`;
  }

  function selectOptions(values, selected) {
    return values
      .map((value) => `<option value="${value}" ${value === selected ? "selected" : ""}>${escapeHtml(value)}</option>`)
      .join("");
  }

  function formatSoundValue(value, unit) {
    const number = Number(value);
    if (unit === "%") return `${Number((number * 100).toFixed(4))}%`;
    if (unit === "Hz") return `${Number(number.toFixed(6))} Hz`;
    if (unit === "s") return `${Number(number.toFixed(6))} s`;
    return String(value);
  }

  function validateSoundToolControl(control) {
    const maximum = Number(control.dataset.maximumExclusive);
    if (!Number.isFinite(maximum)) return;
    const valid = Number.isFinite(control.valueAsNumber) && control.valueAsNumber < maximum;
    control.setCustomValidity(valid ? "" : `Enter a value below ${maximum}`);
  }

  function regionalEffectsForTrack(track) {
    const effects = [];
    for (const edit of state.project.edits) collectRegionalEffects(edit.action, track.role, edit, effects);
    return effects;
  }

  function collectRegionalEffects(action, role, edit, effects) {
    if (action.type === "compound") {
      for (const child of action.actions) collectRegionalEffects(child, role, edit, effects);
      return;
    }
    if (action.type === "timed") {
      const duration = edit.end - edit.start;
      collectRegionalEffects(action.action, role, {
        start: edit.start + duration * action.start,
        end: edit.start + duration * action.end,
      }, effects);
      return;
    }
    if (action.type === "effect" && (action.target === "all" || action.target === role)) {
      effects.push({ name: action.name, detail: `${Math.round(action.value * 100)}%`, start: edit.start, end: edit.end });
    }
    if (action.type === "remove-effect" && (action.target === "all" || action.target === role)) {
      effects.push({ name: action.name, detail: "OFF", start: edit.start, end: edit.end });
    }
    if (action.type === "filter" && (action.target === "all" || action.target === role)) {
      const amount = Math.round(action.value * 100);
      effects.push({
        name: "Tone filter",
        detail: `${amount > 0 ? "+" : ""}${amount}%`,
        start: edit.start,
        end: edit.end,
      });
    }
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

  function changeMix(trackId, values, focusControl) {
    return enqueueProjectMutation(() => applyMixChange(trackId, values, focusControl));
  }

  function setChannelMutationPending(pending) {
    state.channelMutationPending = pending;
    elements.addChannel.disabled = pending;
    elements.channelList.querySelectorAll("[data-delete-track]").forEach((button) => {
      button.disabled = pending;
    });
  }

  function channelMutationRecord(project, clientOperationId, action, values) {
    const record = project.channelOperations?.find(
      (operation) => operation.operationId === clientOperationId,
    );
    if (!record || record.action !== action) return null;
    if (action === "add" && record.role !== values.role) return null;
    if (action === "delete" && String(record.trackId) !== String(values.track_id)) return null;
    return record;
  }

  function focusChannelMutation(operation) {
    if (operation.action === "add") {
      elements.channelList
        .querySelector(`[data-channel-track="${operation.trackId}"]`)
        ?.focus({ preventScroll: true });
    } else {
      elements.addChannel.focus({ preventScroll: true });
    }
  }

  function changeChannel(action, values) {
    if (state.channelMutationPending) return Promise.resolve();
    const clientOperationId = operationId();
    setChannelMutationPending(true);
    return enqueueProjectMutation(async () => {
      let reconciled = true;
      let confirmedOperation = null;
      try {
        await replaceProject(async () => {
          const project = await api("/api/channels", {
            method: "POST",
            headers: { "Content-Type": "application/x-www-form-urlencoded" },
            body: new URLSearchParams({ operation_id: clientOperationId, action, ...values }),
          });
          confirmedOperation = channelMutationRecord(
            project,
            clientOperationId,
            action,
            values,
          );
          if (!confirmedOperation) throw new Error("The track response did not identify this mutation.");
          adoptProject(project);
          renderProject();
        });
        showToast(action === "add" ? "Track added" : "Track deleted");
      } catch (error) {
        if (isRetryableApiError(error)) {
          try {
            await replaceProject(async () => {
              adoptProject(await api("/api/project", {}, RECONCILED_REQUEST_TIMEOUT_MS));
              renderProject();
            });
            confirmedOperation = channelMutationRecord(
              state.project,
              clientOperationId,
              action,
              values,
            );
            if (confirmedOperation) {
              showToast(action === "add" ? "Track added" : "Track deleted");
            } else {
              showError(error, action === "add" ? "adding a track" : "deleting a track");
            }
          } catch (refreshError) {
            reconciled = false;
            showError(
              new Error(
                `The track result could not be confirmed. Reload before trying again. ${errorMessage(refreshError)}`,
              ),
              action === "add" ? "adding a track" : "deleting a track",
            );
          }
        } else {
          showError(error, action === "add" ? "adding a track" : "deleting a track");
        }
      } finally {
        if (reconciled) {
          setChannelMutationPending(false);
          if (confirmedOperation) focusChannelMutation(confirmedOperation);
        }
      }
    });
  }

  function changeSoundTool(control, value) {
    if (control.dataset.soundTool === "instrument") {
      state.graphNodeSelection[control.dataset.trackId] = `instrument:${control.dataset.toolId}`;
    } else if (control.dataset.soundTool === "effect" || control.dataset.soundTool === "routing") {
      state.graphNodeSelection[control.dataset.trackId] = `effect:${control.dataset.toolId}`;
    } else if (control.dataset.soundTool === "modulator") {
      state.graphNodeSelection[control.dataset.trackId] = `modulator:${control.dataset.toolId}`;
    }
    const request = {
      track_id: control.dataset.trackId,
      tool: control.dataset.soundTool,
      tool_id: control.dataset.toolId,
      parameter: control.dataset.parameter,
      value: String(value),
    };
    if (control.dataset.clipId) request.clip_id = control.dataset.clipId;
    if (!control.checkValidity()) {
      renderProject();
      restoreSoundToolFocus(request, control.dataset.controlKey);
      showToast("Enter a value within the supported range", true);
      return Promise.resolve();
    }
    return enqueueProjectMutation(() => applySoundToolChange(request, control.dataset.controlKey));
  }

  function restoreSoundToolFocus(request, focusKey) {
    const controls = [...elements.channelList.querySelectorAll("[data-control-key]")];
    const exactControl = controls.find((candidate) => candidate.dataset.controlKey === focusKey);
    const sameToolControls = controls.filter(
      (candidate) =>
        candidate.dataset.trackId === request.track_id &&
        candidate.dataset.toolId === request.tool_id &&
        !candidate.disabled,
    );
    const fallbackControl =
      sameToolControls.find((candidate) => candidate.dataset.soundTool === request.tool) ?? sameToolControls[0];
    const focusControl = exactControl && !exactControl.disabled ? exactControl : fallbackControl;
    if (!focusControl) return;
    focusControl.focus({ preventScroll: true });
  }

  async function applySoundToolChange(request, focusKey) {
    await applyProjectMutation({
      path: "/api/sound-tools",
      values: request,
      context: "updating a sound tool",
      successMessage: "Sound tool updated",
      commitUi: () => {
        if (request.tool !== "instrument") return;
        if (request.parameter.startsWith("native:")) {
          for (const group of ["common", "advanced"]) {
            const parameter = state.instrumentParameters[`${request.track_id}:${group}`]
              ?.find((candidate) => candidate.parameter === request.parameter);
            if (parameter) {
              parameter.value = Number(request.value);
              parameter.overridden = true;
            }
          }
        } else if (request.parameter === "preset") {
          delete state.instrumentParameters[`${request.track_id}:common`];
          delete state.instrumentParameters[`${request.track_id}:advanced`];
        }
      },
      restoreUi: () => restoreSoundToolFocus(request, focusKey),
      renderOnFailure: true,
    });
  }

  async function applyMixChange(trackId, values, focusControl) {
    await applyProjectMutation({
      path: "/api/mix",
      values: { track_id: trackId, ...values },
      context: "updating the mixer",
      restoreUi: () => {
        const selector =
          focusControl === "volume" ? `[data-volume-track="${trackId}"]` : `[data-mute-track="${trackId}"]`;
        elements.channelList.querySelector(selector)?.focus({ preventScroll: true });
      },
    });
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
    const detail = job.detail || "Gemini is working on the edit";
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
      if (validOperationId && validRequest && validJob) return pending;
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
    } catch (error) {
      reportClientIssue("warning", error, "persisting an active edit");
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
        : operation.message || "Gemini stopped before completing the edit.",
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
          await refreshAuthoritativeProject("Gemini stopped; refreshing its partial changes");
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
        accepted = await acceptEdit(
          clientOperationId,
          new URLSearchParams({
            operation_id: clientOperationId,
            prompt,
            start: String(selectionStart),
            end: String(selectionEnd),
          }),
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
    const submittedText = elements.promptInput.value;
    const prompt = submittedText.trim();
    if (!prompt) return;
    const pending = {
      operationId: operationId(),
      prompt,
      submittedText,
      start: state.selectionStart,
      end: state.selectionEnd,
      acceptedJob: null,
    };
    persistPendingEdit(pending);
    await runPendingEdit(pending, true);
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
          state.selectionStart = 8;
          state.selectionEnd = 16;
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
      { name: "advanced", button: elements.advancedButton, panel: elements.advancedDrawer },
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
    elements.advancedDrawer.classList.toggle("is-open", view === "advanced");
    if (view === "debug") renderDebug();
    if (view === "ai" && state.project) {
      renderSelection();
      renderPlayhead();
    }
    window.scrollTo(0, 0);
  }

  function openAdvanced() {
    setView("advanced");
  }

  function closeAdvanced() {
    setView("ai");
    elements.aiModeButton.focus();
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
    const tabs = [elements.aiModeButton, elements.advancedButton, elements.debugButton];
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
      `Gemini sessions: ${state.geminiSessions.length} retained locally`,
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
    lines.push("", "Recent Gemini sessions:");
    if (state.geminiSessions.length === 0) {
      lines.push("None found.");
    } else {
      for (const session of state.geminiSessions.slice(0, 10)) {
        lines.push(
          `${new Date(Number(session.createdAt) || 0).toISOString()} [${session.status || "unknown"}] ` +
            `${session.appliedSteps || 0} edit actions, ${session.audioListens || 0} listens: ${session.prompt || ""}`,
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

  function renderDebug() {
    elements.debugReport.value = debugReport();
    if (state.geminiSessions.length === 0) {
      elements.geminiSessionList.innerHTML = '<div class="empty-log">No Gemini sessions recorded yet.</div>';
      return;
    }
    elements.geminiSessionList.innerHTML = state.geminiSessions
      .slice(0, 20)
      .map(
        (session) => `<article class="gemini-session-item">
          <div><strong>${escapeHtml(new Date(Number(session.createdAt) || 0).toLocaleString())}</strong>
          <span>${escapeHtml(session.status || "unknown")} &middot; ${Number(session.appliedSteps) || 0} actions &middot; ${Number(session.audioListens) || 0} listens</span></div>
          <p>${escapeHtml(session.prompt || "Untitled edit")}</p>
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
      showError(error, "loading Gemini sessions");
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
  elements.resetButton.addEventListener("click", () => void reset());
  elements.selectionModeButton.addEventListener("click", () => setTouchSelectionMode(!state.touchSelectionMode));
  elements.skipLink.addEventListener("click", skipToTimeline);
  elements.aiModeButton.addEventListener("click", () => setView("ai"));
  elements.advancedButton.addEventListener("click", openAdvanced);
  elements.debugButton.addEventListener("click", openDebug);
  [elements.aiModeButton, elements.advancedButton, elements.debugButton].forEach((button) => {
    button.addEventListener("keydown", handleViewTabKey);
  });
  elements.closeAdvanced.addEventListener("click", closeAdvanced);
  elements.channelCreator.addEventListener("submit", (event) => {
    event.preventDefault();
    void changeChannel("add", { role: elements.channelRole.value });
  });
  elements.copyDebug.addEventListener("click", () => void copyDebugReport());
  elements.clearDebug.addEventListener("click", clearDebugIssues);
  elements.refreshGeminiSessions.addEventListener("click", () => void loadGeminiSessions());
  elements.audioBackend.addEventListener("change", () => void changeAudioBackend());
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
    elements.playButton.disabled = !(audio.streamToken && state.project);
    await loadGeminiSessions();
    if (pending && state.project) await runPendingEdit(pending, false);
  }

  void initialize();
})();
