"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");
const { once } = require("node:events");

const WebSocketClient = globalThis.WebSocket || require("undici").WebSocket;
const root = path.resolve(__dirname, "..");

class CdpClient {
  static async connect(url) {
    const client = new CdpClient(url);
    await client.ready;
    return client;
  }

  constructor(url) {
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Map();
    this.socket = new WebSocketClient(url);
    this.ready = new Promise((resolve, reject) => {
      this.socket.addEventListener("open", resolve, { once: true });
      this.socket.addEventListener("error", reject, { once: true });
    });
    this.socket.addEventListener("message", (event) => this.handleMessage(event.data));
    this.socket.addEventListener("close", () => {
      for (const { reject } of this.pending.values()) reject(new Error("Chrome DevTools connection closed"));
      this.pending.clear();
    });
  }

  handleMessage(data) {
    const text = typeof data === "string" ? data : Buffer.from(data).toString("utf8");
    const message = JSON.parse(text);
    if (message.id) {
      const pending = this.pending.get(message.id);
      if (!pending) return;
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result);
      return;
    }
    for (const listener of this.listeners.get(message.method) || []) listener(message);
  }

  async send(method, params = {}, sessionId = undefined) {
    await this.ready;
    const id = this.nextId;
    this.nextId += 1;
    const message = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    const result = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.socket.send(JSON.stringify(message));
    return result;
  }

  on(method, listener) {
    const listeners = this.listeners.get(method) || [];
    listeners.push(listener);
    this.listeners.set(method, listeners);
  }

  close() {
    this.socket.close();
  }
}

function reservePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => {
      const { port } = server.address();
      server.close((error) => (error ? reject(error) : resolve(port)));
    });
  });
}

function findBrowser() {
  const candidates = [
    process.env.CHROME_PATH,
    "/usr/bin/google-chrome",
    "/usr/bin/google-chrome-stable",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
  ].filter(Boolean);
  const playwrightCache = path.join(os.homedir(), ".cache", "ms-playwright");
  if (fs.existsSync(playwrightCache)) findCachedBrowsers(playwrightCache, 0, candidates);
  return candidates.find((candidate) => {
    try {
      fs.accessSync(candidate, fs.constants.X_OK);
      return true;
    } catch (_error) {
      return false;
    }
  });
}

function findCachedBrowsers(directory, depth, candidates) {
  if (depth > 3) return;
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    const entryPath = path.join(directory, entry.name);
    if (entry.isDirectory()) findCachedBrowsers(entryPath, depth + 1, candidates);
    if (entry.isFile() && (entry.name === "chrome" || entry.name === "headless_shell")) candidates.push(entryPath);
  }
}

async function waitFor(check, description, timeout = 12000) {
  const deadline = Date.now() + timeout;
  let lastError;
  while (Date.now() < deadline) {
    try {
      const value = await check();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await new Promise((resolve) => setTimeout(resolve, 40));
  }
  throw new Error(`Timed out waiting for ${description}${lastError ? `: ${lastError.message}` : ""}`);
}

async function openPage(cdp, url) {
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  await cdp.send("Runtime.enable", {}, sessionId);
  await cdp.send("Page.enable", {}, sessionId);
  await cdp.send(
    "Emulation.setDeviceMetricsOverride",
    { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false },
    sessionId,
  );
  await cdp.send("Page.navigate", { url }, sessionId);
  await waitFor(
    async () => evaluate(cdp, sessionId, "document.readyState === 'complete'"),
    `page load for ${url}`,
  );
  return sessionId;
}

async function openPageWithScript(cdp, url, source) {
  const { targetId } = await cdp.send("Target.createTarget", { url: "about:blank" });
  const { sessionId } = await cdp.send("Target.attachToTarget", { targetId, flatten: true });
  await cdp.send("Runtime.enable", {}, sessionId);
  await cdp.send("Page.enable", {}, sessionId);
  await cdp.send(
    "Emulation.setDeviceMetricsOverride",
    { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false },
    sessionId,
  );
  await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source }, sessionId);
  await cdp.send("Page.navigate", { url }, sessionId);
  await waitFor(
    async () => evaluate(cdp, sessionId, "document.readyState === 'complete'"),
    `scripted page load for ${url}`,
  );
  return { sessionId, targetId };
}

async function evaluate(cdp, sessionId, expression) {
  const response = await cdp.send(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true, userGesture: true },
    sessionId,
  );
  if (response.exceptionDetails) {
    const detail = response.exceptionDetails.exception?.description || response.exceptionDetails.text;
    throw new Error(detail);
  }
  return response.result.value;
}

async function mouse(cdp, sessionId, type, x, y, buttons = 0, clickCount = 1) {
  await cdp.send(
    "Input.dispatchMouseEvent",
    { type, x, y, button: "left", buttons, clickCount },
    sessionId,
  );
}

async function touch(cdp, sessionId, type, touchPoints) {
  await cdp.send("Input.dispatchTouchEvent", { type, touchPoints }, sessionId);
  await new Promise((resolve) => setTimeout(resolve, 35));
}

async function pressKey(cdp, sessionId, key, code, virtualKey, modifiers = 0) {
  const values = {
    key,
    code,
    modifiers,
    windowsVirtualKeyCode: virtualKey,
    nativeVirtualKeyCode: virtualKey,
  };
  await cdp.send("Input.dispatchKeyEvent", { type: "rawKeyDown", ...values }, sessionId);
  await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", ...values }, sessionId);
}

async function submitPrompt(cdp, sessionId, prompt, expectedEditCount) {
  await evaluate(cdp, sessionId, `(() => {
    const input = document.querySelector('#prompt-input');
    input.value = ${JSON.stringify(prompt)};
    document.querySelector('#prompt-form').requestSubmit();
  })()`);
  await waitFor(
    async () =>
      evaluate(cdp, sessionId, `Number(document.querySelector('#session-history-list').dataset.currentEditCount) === ${expectedEditCount}`),
    `prompt: ${prompt}`,
  );
}

async function startAttackerServer(port) {
  const server = http.createServer((_request, response) => {
    response.writeHead(200, { "Content-Type": "text/html" });
    response.end("<!doctype html><title>Untrusted origin</title>");
  });
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  return server;
}

async function terminate(child) {
  if (child.exitCode !== null || child.signalCode !== null) return;
  const exited = once(child, "exit");
  child.kill("SIGTERM");
  const timeout = new Promise((resolve) => setTimeout(resolve, 2000, false));
  if ((await Promise.race([exited.then(() => true), timeout])) === false) {
    child.kill("SIGKILL");
    await once(child, "exit");
  }
}

async function closeBrowser(cdp, child) {
  if (!cdp) {
    await terminate(child);
    return;
  }
  await Promise.race([
    cdp.send("Browser.close").catch(() => {}),
    new Promise((resolve) => setTimeout(resolve, 2000)),
  ]);
  cdp.close();
  if (child.exitCode === null && child.signalCode === null) {
    const exited = once(child, "exit").then(() => true);
    const timeout = new Promise((resolve) => setTimeout(resolve, 2000, false));
    if ((await Promise.race([exited, timeout])) === false) await terminate(child);
  }
}

async function removeBrowserProfile(directory) {
  const retryableErrors = new Set(["EACCES", "EBUSY", "ENOTEMPTY", "EPERM"]);
  let lastError;
  for (let attempt = 0; attempt < 80; attempt += 1) {
    try {
      fs.rmSync(directory, { recursive: true, force: true });
      return;
    } catch (error) {
      if (!retryableErrors.has(error.code)) throw error;
      lastError = error;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
  }
  throw lastError;
}

async function run() {
  const browserPath = findBrowser();
  if (!browserPath) {
    throw new Error("Chrome or Chromium is required. Set CHROME_PATH to its executable.");
  }
  if (process.argv.includes("--check-browser")) {
    console.log(browserPath);
    return;
  }

  const appPort = await reservePort();
  const debugPort = await reservePort();
  const attackerPort = await reservePort();
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "daw-ai-browser-"));
  const appEnvironment = { ...process.env };
  delete appEnvironment.DAW_AI_GEMINI_API_KEY;
  delete appEnvironment.DAW_AI_GEMINI_CREDENTIALS;
  delete appEnvironment.GEMINI_API_KEY;
  const app = spawn(path.join(root, "target", "debug", "daw-ai"), ["--port", String(appPort)], {
    cwd: root,
    env: {
      ...appEnvironment,
      DAW_AI_PROMPT_ENGINE: "demo",
      DAW_AI_PROJECT_PATH: path.join(profile, "sound-graph.json"),
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  const chrome = spawn(
    browserPath,
    [
      "--headless",
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--mute-audio",
      `--remote-debugging-port=${debugPort}`,
      `--user-data-dir=${profile}`,
      "about:blank",
    ],
    { stdio: ["ignore", "ignore", "pipe"] },
  );
  let attacker;
  let cdp;
  let appErrors = "";
  let chromeErrors = "";
  app.stderr.on("data", (chunk) => {
    appErrors += chunk;
  });
  chrome.stderr.on("data", (chunk) => {
    chromeErrors += chunk;
  });

  try {
    await waitFor(
      async () => fetch(`http://127.0.0.1:${appPort}/api/health`).then((response) => response.ok),
      "Rust server",
    );
    const browserWebSocket = await waitFor(
      async () => {
        const response = await fetch(`http://127.0.0.1:${debugPort}/json/version`);
        if (!response.ok) return false;
        return (await response.json()).webSocketDebuggerUrl;
      },
      "Chrome DevTools endpoint",
      30_000,
    ).catch((error) => {
      const details = chromeErrors.trim();
      if (!details) throw error;
      throw new Error(`${error.message}\nChrome stderr:\n${details}`);
    });
    cdp = await CdpClient.connect(browserWebSocket);
    const appUrl = `http://127.0.0.1:${appPort}`;
    const startupPage = await openPageWithScript(cdp, appUrl, `(() => {
      const originalFetch = window.fetch;
      window.__initialProjectPending = false;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/project') return originalFetch(resource, options);
        window.__initialProjectPending = true;
        return new Promise((resolve, reject) => {
          window.__releaseInitialProject = () => originalFetch(resource, options).then(resolve, reject);
        });
      };
    })()`);
    await waitFor(
      async () => evaluate(cdp, startupPage.sessionId, "window.__initialProjectPending"),
      "delayed initial project request",
    );
    assert.deepEqual(
      await evaluate(cdp, startupPage.sessionId, `(() => {
        const play = document.querySelector('#play-button');
        play.click();
        return { disabled: play.disabled, playing: play.classList.contains('is-playing') };
      })()`),
      { disabled: true, playing: false },
      "Play must remain disabled until audio access and the project are both ready",
    );
    await evaluate(cdp, startupPage.sessionId, "window.__releaseInitialProject() ");
    await waitFor(
      async () => evaluate(
        cdp,
        startupPage.sessionId,
        "!document.querySelector('#play-button').disabled && document.querySelectorAll('.track-row').length === 3",
      ),
      "playback readiness after initial project load",
      60_000,
    );
    await cdp.send("Target.closeTarget", { targetId: startupPage.targetId });
    const playbackPriorityPage = await openPageWithScript(cdp, appUrl, `(() => {
      const originalFetch = window.fetch;
      window.__playbackDrivenSpectrumPending = false;
      window.fetch = function fetch(resource, options) {
        if (typeof resource === 'string' && resource.startsWith('/api/track-spectrum/')) {
          window.__playbackDrivenSpectrumPending = true;
          return new Promise(() => {});
        }
        return originalFetch(resource, options);
      };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        playbackPriorityPage.sessionId,
        `!document.querySelector('#play-button').disabled`,
      ),
      "playback readiness during spectrum prefetch",
    );
    assert.equal(
      await evaluate(cdp, playbackPriorityPage.sessionId, "window.__playbackDrivenSpectrumPending"),
      true,
      "the opening spectrum must begin warming when the project is adopted",
    );
    await evaluate(cdp, playbackPriorityPage.sessionId, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        playbackPriorityPage.sessionId,
        `document.documentElement.dataset.audioState === 'playing' &&
          window.__playbackDrivenSpectrumPending`,
      ),
      "playback while the opening spectrum is still warming",
      30_000,
    );
    await cdp.send("Target.closeTarget", { targetId: playbackPriorityPage.targetId });
    const degradedSpectrumPage = await openPageWithScript(cdp, appUrl, `(() => {
      const originalFetch = window.fetch;
      window.__failedSpectrumRequests = 0;
      window.fetch = function fetch(resource, options) {
        if (typeof resource === 'string' && resource.startsWith('/api/track-spectrum/')) {
          window.__failedSpectrumRequests += 1;
          return Promise.resolve(new Response('{"error":"simulated spectrum failure"}', {
            status: 503,
            headers: { 'Content-Type': 'application/json' },
          }));
        }
        return originalFetch(resource, options);
      };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        degradedSpectrumPage.sessionId,
        `!document.querySelector('#play-button').disabled && window.__failedSpectrumRequests >= 1`,
      ),
      "transport readiness while spectrum warming is degraded",
    );
    await evaluate(cdp, degradedSpectrumPage.sessionId, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        degradedSpectrumPage.sessionId,
        `document.documentElement.dataset.audioState === 'playing' && window.__failedSpectrumRequests >= 1`,
      ),
      "playback while spectrum preparation is degraded",
      30_000,
    );
    await waitFor(
      async () => evaluate(cdp, degradedSpectrumPage.sessionId, "window.__failedSpectrumRequests >= 3"),
      "independent spectrum retry during playback",
      10_000,
    );
    await cdp.send("Target.closeTarget", { targetId: degradedSpectrumPage.targetId });
    const longProjectPage = await openPageWithScript(cdp, appUrl, `(() => {
      const originalFetch = window.fetch;
      window.__spectrumStarts = [];
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/project') {
          const response = await originalFetch(resource, options);
          const project = await response.json();
          project.duration = 65;
          return new Response(JSON.stringify(project), {
            status: response.status,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        if (typeof resource === 'string' && resource.startsWith('/api/track-spectrum/')) {
          const parts = resource.split('/');
          const startMilliseconds = Number(parts[5]);
          const requestedMilliseconds = parts[6] === undefined ? 64000 : Number(parts[6]);
          const durationMilliseconds = Math.min(64000, requestedMilliseconds);
          const project = await originalFetch('/api/project').then((response) => response.json());
          const frameSamples = 1470;
          const sampleRate = 44100;
          const frameCount = Math.ceil(durationMilliseconds / 1000 * sampleRate / frameSamples);
          const trackCount = project.tracks.length;
          const buffer = new ArrayBuffer(32 + trackCount * 8 + frameCount * trackCount * 8);
          const bytes = new Uint8Array(buffer);
          const view = new DataView(buffer);
          bytes.set([...'DAWSPEC1'].map((character) => character.charCodeAt(0)));
          view.setUint32(8, trackCount, true);
          view.setUint32(12, frameCount, true);
          view.setBigUint64(16, BigInt(startMilliseconds), true);
          view.setUint32(24, frameSamples, true);
          view.setUint32(28, sampleRate, true);
          project.tracks.forEach((track, index) => view.setBigUint64(32 + index * 8, BigInt(track.id), true));
          window.__spectrumStarts.push(startMilliseconds);
          return new Response(buffer, {
            status: 200,
            headers: { 'Content-Type': 'application/vnd.daw-ai.track-spectrum' },
          });
        }
        return originalFetch(resource, options);
      };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        longProjectPage.sessionId,
        `!document.querySelector('#play-button').disabled && window.__spectrumStarts.length === 1`,
      ),
      "opening spectrum warmup for a project longer than one window",
      10_000,
    );
    await evaluate(cdp, longProjectPage.sessionId, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        longProjectPage.sessionId,
        `document.documentElement.dataset.audioState === 'playing' && window.__spectrumStarts.length === 1`,
      ),
      "opening spectrum window after long-project playback starts",
      30_000,
    );
    assert.deepEqual(
      await evaluate(cdp, longProjectPage.sessionId, "window.__spectrumStarts"),
      [0],
      "long projects must warm only their opening spectrum window",
    );
    await cdp.send("Target.closeTarget", { targetId: longProjectPage.targetId });
    const delayedPrefetchPage = await openPageWithScript(cdp, appUrl, `(() => {
      const originalFetch = window.fetch;
      window.__spectrumTrackIds = [];
      window.__spectrumRequestCount = 0;
      window.__futureSpectrumPending = false;
      const spectrumResponse = (startMilliseconds) => {
        const frameSamples = 1470;
        const sampleRate = 44100;
        const frameCount = 90;
        const trackCount = window.__spectrumTrackIds.length;
        const buffer = new ArrayBuffer(32 + trackCount * 8 + frameCount * trackCount * 8);
        const bytes = new Uint8Array(buffer);
        const view = new DataView(buffer);
        bytes.set([...'DAWSPEC1'].map((character) => character.charCodeAt(0)));
        view.setUint32(8, trackCount, true);
        view.setUint32(12, frameCount, true);
        view.setBigUint64(16, BigInt(startMilliseconds), true);
        view.setUint32(24, frameSamples, true);
        view.setUint32(28, sampleRate, true);
        window.__spectrumTrackIds.forEach(
          (trackId, index) => view.setBigUint64(32 + index * 8, BigInt(trackId), true),
        );
        bytes.fill(192, 32 + trackCount * 8);
        return new Response(buffer, {
          status: 200,
          headers: { 'Content-Type': 'application/vnd.daw-ai.track-spectrum' },
        });
      };
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/project') {
          const response = await originalFetch(resource, options);
          const project = await response.json();
          window.__spectrumTrackIds = project.tracks.map((track) => track.id);
          return new Response(JSON.stringify(project), {
            status: response.status,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        if (typeof resource === 'string' && resource.startsWith('/api/track-spectrum/')) {
          const startMilliseconds = Number(resource.split('/')[5]);
          window.__spectrumRequestCount += 1;
          if (window.__spectrumRequestCount === 1) return spectrumResponse(startMilliseconds);
          window.__futureSpectrumPending = true;
          return new Promise((resolve) => {
            window.__releaseFutureSpectrum = () => {
              window.__futureSpectrumPending = false;
              resolve(spectrumResponse(startMilliseconds));
            };
          });
        }
        return originalFetch(resource, options);
      };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        delayedPrefetchPage.sessionId,
        `!document.querySelector('#play-button').disabled && window.__spectrumRequestCount === 1`,
      ),
      "playback readiness after opening spectrum prefetch",
    );
    await evaluate(cdp, delayedPrefetchPage.sessionId, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        delayedPrefetchPage.sessionId,
        `document.documentElement.dataset.audioState === 'playing' && window.__futureSpectrumPending`,
      ),
      "delayed future spectrum prefetch",
      30_000,
    );
    const delayedPrefetchBaseline = await evaluate(
      cdp,
      delayedPrefetchPage.sessionId,
      "Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0)",
    );
    await evaluate(cdp, delayedPrefetchPage.sessionId, "new Promise((resolve) => setTimeout(resolve, 500))");
    assert.equal(
      await evaluate(cdp, delayedPrefetchPage.sessionId, `(() => {
        const frame = Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0);
        const activeBar = [...document.querySelectorAll('.track-spectrum i')].some(
          (bar) => Number.parseFloat(bar.style.getPropertyValue('--spectrum-level')) > 0.1,
        );
        return frame >= ${delayedPrefetchBaseline + 10} && activeBar;
      })()`),
      true,
      "current analyzer animation must continue while a future window is rendering",
    );
    await evaluate(cdp, delayedPrefetchPage.sessionId, "window.__releaseFutureSpectrum()");
    await cdp.send("Target.closeTarget", { targetId: delayedPrefetchPage.targetId });
    const appSession = await openPage(cdp, appUrl);
    const consoleErrors = [];
    cdp.on("Runtime.consoleAPICalled", (message) => {
      if (message.sessionId === appSession && message.params.type === "error") consoleErrors.push(message.params);
    });
    cdp.on("Runtime.exceptionThrown", (message) => {
      if (message.sessionId === appSession) consoleErrors.push(message.params);
    });

    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelectorAll('.track-row').length === 3"),
      "initial arrangement",
    );
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        "document.querySelector('#track-rows').dataset.spectrumCoverage !== undefined",
      ),
      "opening analyzer window warmup",
      60_000,
    );
    const timelineMidi = await evaluate(cdp, appSession, `(async () => {
        const project = await fetch('/api/project').then((response) => response.json());
        const graphEvents = project.tracks.flatMap((track) => track.clips.flatMap((clip) => clip.events));
        const notes = [...document.querySelectorAll('.timeline-midi i')];
        const levels = notes.map((note) => Number(note.style.getPropertyValue('--timeline-note-level')));
        const positions = notes.map((note) => Number.parseFloat(note.style.getPropertyValue('--timeline-note-left')));
        return {
          fakeWaveforms: document.querySelectorAll('.waveform').length,
          graphEvents: graphEvents.length,
          renderedNotes: notes.length,
          hasLoopedOccurrences: positions.some((position) => position > 50),
          hasDynamicBrightness: Math.max(...levels) > Math.min(...levels),
        };
      })()`);
    assert.equal(timelineMidi.fakeWaveforms, 0);
    assert.ok(timelineMidi.graphEvents > 0);
    assert.ok(timelineMidi.renderedNotes > timelineMidi.graphEvents);
    assert.equal(timelineMidi.hasLoopedOccurrences, true);
    assert.equal(timelineMidi.hasDynamicBrightness, true);
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        historyHeading: document.querySelector('#session-history-heading').textContent,
        duplicateGeminiHistory: Boolean(document.querySelector('#edit-log-title')),
        suggestions: [...document.querySelectorAll('.prompt-suggestions button')]
          .map((button) => ({ label: button.textContent, prompt: button.dataset.prompt })),
      })`),
      {
        historyHeading: "History",
        duplicateGeminiHistory: false,
        suggestions: [
          { label: "Waltz", prompt: "Turn this section into a waltz" },
          {
            label: "Drop",
            prompt: "Turn this section into a classic dubstep drop. It builds in speed and intensity for the first 80%, then the drop, and then the outro into the rest of the track",
          },
          { label: "warm", prompt: "Make the chords warm and spacious, and this section relaxing" },
        ],
      },
      "history and prompt suggestions",
    );
    const offlineRender = await evaluate(
      cdp,
      appSession,
      `(async () => {
        const project = await fetch('/api/project').then((response) => response.json());
        const access = await fetch('/api/audio-access', {
          headers: { 'X-DAW-AI-Audio': '1' },
        }).then((response) => response.json());
        const duration = 16;
        const lastByte = 44 + duration * 16000 * 2 - 1;
        const response = await fetch(
          '/api/audio-stream/' + encodeURIComponent(access.streamToken) + '/' + project.version + '/0',
          { headers: { Range: 'bytes=0-' + lastByte } },
        );
        const bytes = new Uint8Array(await response.arrayBuffer());
        const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
        let maximum = 0;
        for (let offset = 44; offset < bytes.length; offset += 2) {
          maximum = Math.max(maximum, Math.abs(view.getInt16(offset, true)));
        }
        return {
          riff: String.fromCharCode(...bytes.slice(0, 4)),
          wave: String.fromCharCode(...bytes.slice(8, 12)),
          channels: view.getUint16(22, true),
          sampleRate: view.getUint32(24, true),
          length: bytes.length,
          expectedLength: lastByte + 1,
          projectVersion: project.version,
          expectedVersion: project.version,
          start: 0,
          end: duration,
          maximum,
        };
      })()`,
    );
    assert.deepEqual(offlineRender, {
      riff: "RIFF",
      wave: "WAVE",
      channels: 2,
      sampleRate: 16000,
      length: offlineRender.expectedLength,
      expectedLength: offlineRender.expectedLength,
      projectVersion: offlineRender.expectedVersion,
      expectedVersion: offlineRender.expectedVersion,
      start: 0,
      end: 16,
      maximum: offlineRender.maximum,
    });
    assert.ok(offlineRender.maximum > 100, "backend render should contain music");
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        containerRole: document.querySelector('#edit-progress').getAttribute('role'),
        labelRole: document.querySelector('#edit-progress-label').getAttribute('role'),
        labelLive: document.querySelector('#edit-progress-label').getAttribute('aria-live'),
        timerHidden: document.querySelector('#edit-progress-time').getAttribute('aria-hidden'),
        progressLabel: document.querySelector('#edit-progress-track').getAttribute('aria-label'),
        progressValue: document.querySelector('#edit-progress-track').getAttribute('aria-valuenow'),
      })`),
      {
        containerRole: null,
        labelRole: "status",
        labelLive: "polite",
        timerHidden: "true",
        progressLabel: "AI edit activity",
        progressValue: null,
      },
      "open-ended edit activity and elapsed time must have accurate accessibility semantics",
    );
    const { identifier: resumeEditScript } = await cdp.send(
      "Page.addScriptToEvaluateOnNewDocument",
      {
        source: `(() => {
          const originalFetch = window.fetch;
          window.__reloadPollCount = 0;
          window.__releaseReloadJob = false;
          window.fetch = function fetch(resource, options) {
            if (resource !== '/api/edits/reload-job') return originalFetch(resource, options);
            window.__reloadPollCount += 1;
            const job = window.__releaseReloadJob
              ? {
                  id: 'reload-job', operationId: 'reload-operation', status: 'failed', phase: 'failed',
                  errorStatus: 422, error: 'Simulated resumed edit stopped', elapsedSeconds: 14,
                  timeoutSeconds: 1200,
                }
              : {
                  id: 'reload-job', operationId: 'reload-operation', status: 'running', phase: 'planning',
                  detail: 'Gemini is planning the reloaded edit', elapsedSeconds: 13,
                  timeoutSeconds: 1200, pollAfterMs: 20,
                };
            return Promise.resolve(new Response(JSON.stringify(job), {
              status: 200,
              headers: { 'Content-Type': 'application/json' },
            }));
          };
        })();`,
      },
      appSession,
    );
    await evaluate(cdp, appSession, `localStorage.setItem('daw-ai.pending-edit.v1', JSON.stringify({
      operationId: 'reload-operation',
      prompt: 'resume after reload',
      submittedText: 'resume after reload',
      start: 8,
      end: 16,
      acceptedJob: {
        id: 'reload-job', operationId: 'reload-operation', status: 'running', phase: 'planning',
        detail: 'Gemini is planning the reloaded edit', elapsedSeconds: 13,
        timeoutSeconds: 1200, pollAfterMs: 20,
      },
    }))`);
    await cdp.send("Page.reload", { ignoreCache: true }, appSession);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `window.__reloadPollCount >= 1 &&
          !document.querySelector('#compose-button').disabled &&
          document.querySelector('#compose-button span').textContent === 'Interrupt' &&
          !document.querySelector('#edit-progress').hidden &&
          document.querySelector('#edit-progress-label').textContent === 'Gemini is planning the reloaded edit' &&
          document.querySelector('#prompt-input').value === 'resume after reload'`,
      ),
      "pending edit recovery after page reload",
    );
    await evaluate(cdp, appSession, "window.__releaseReloadJob = true");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `!document.querySelector('#compose-button').disabled &&
          document.querySelector('#edit-progress').hidden &&
          document.querySelector('#toast-message').textContent === 'Simulated resumed edit stopped' &&
          localStorage.getItem('daw-ai.pending-edit.v1') === null`,
      ),
      "resumed edit terminal cleanup",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').disabled"),
      "playback readiness after resumed edit cleanup",
      60_000,
    );
    await cdp.send("Page.removeScriptToEvaluateOnNewDocument", { identifier: resumeEditScript }, appSession);
    await evaluate(cdp, appSession, "document.querySelector('#prompt-input').value = ''");
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__clientLogBodies = [];
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/logs') return originalFetch(resource, options);
        window.__clientLogBodies.push(options.body.toString());
        return Promise.resolve(new Response('{"status":"logged"}', {
          status: 200,
          headers: { 'Content-Type': 'application/json' },
        }));
      };
      window.__restoreFetchAfterClientLog = () => {
        window.fetch = originalFetch;
      };
      window.dispatchEvent(new ErrorEvent('error', {
        message: 'Synthetic browser failure',
        error: new Error('Synthetic browser failure'),
      }));
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__clientLogBodies.length === 1"),
      "client error forwarding",
    );
    const clientLog = new URLSearchParams(
      await evaluate(cdp, appSession, "window.__clientLogBodies[0]"),
    );
    assert.equal(clientLog.get("level"), "error");
    assert.equal(clientLog.get("context"), "uncaught browser error");
    assert.match(clientLog.get("message"), /Synthetic browser failure/);
    await evaluate(cdp, appSession, "window.__restoreFetchAfterClientLog()");
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#advanced-button, #advanced-drawer')"),
      null,
      "the removed Advanced UI must not be present",
    );
    const debugView = await evaluate(cdp, appSession, `(() => {
      document.querySelector('#debug-button').click();
      return {
        tabs: [...document.querySelectorAll('[role="tab"]')].map((tab) => ({
          name: tab.textContent.trim(),
          selected: tab.getAttribute('aria-selected'),
        })),
        debugVisible: !document.querySelector('#debug-panel').hidden && !document.querySelector('#debug-panel').inert,
        aiHidden: document.querySelector('#ai-mode-panel').hidden && document.querySelector('#ai-mode-panel').inert,
        report: document.querySelector('#debug-report').value,
        panelPadding: parseFloat(getComputedStyle(document.querySelector('#debug-panel')).paddingTop),
        reportHeight: document.querySelector('#debug-report').getBoundingClientRect().height,
        settingsDisplay: getComputedStyle(document.querySelector('.debug-settings')).display,
        batchParameterTools: document.querySelector('#batch-parameter-tools').checked,
        slimPrompt: document.querySelector('#slim-prompt').checked,
        dynamicTools: document.querySelector('#dynamic-tools').checked,
      };
    })()`);
    assert.deepEqual(
      debugView.tabs,
      [
        { name: "AI Mode", selected: "false" },
        { name: "Debug", selected: "true" },
      ],
      "the two chartered studio views must be exposed as tabs",
    );
    assert.equal(debugView.debugVisible && debugView.aiHidden, true, "Debug must replace the AI Mode panel");
    assert.equal(debugView.batchParameterTools, false, "batch parameter tools must default off");
    assert.equal(debugView.slimPrompt, false, "slim prompt must default off");
    assert.equal(debugView.dynamicTools, false, "dynamic tools must default off independently");
    assert.ok(
      debugView.panelPadding >= 20 &&
        debugView.reportHeight >= 400 &&
        debugView.settingsDisplay === "flex",
      `Debug must retain its panel layout (${JSON.stringify(debugView)})`,
    );
    assert.match(debugView.report, /Synthetic browser failure/);
    assert.match(debugView.report, /Backend warnings and errors are written/);
    assert.match(debugView.report, /AI sessions: 0 retained locally/);
    assert.match(
      await evaluate(cdp, appSession, "document.querySelector('#gemini-session-list').textContent"),
      /No AI sessions recorded yet/,
    );
    assert.equal(
      await evaluate(cdp, appSession, `(() => {
        const toggle = document.querySelector('#batch-parameter-tools');
        toggle.click();
        return localStorage.getItem('daw-ai.batch-parameter-tools.v1');
      })()`),
      "true",
      "the Debug experiment toggle must persist locally",
    );
    const sessions = await evaluate(
      cdp,
      appSession,
      "fetch('/api/gemini-sessions').then((response) => response.json())",
    );
    assert.deepEqual(sessions, { sessions: [] }, "AI sessions must be persistently listable");
    await evaluate(cdp, appSession, `(() => {
      Object.defineProperty(navigator, 'clipboard', {
        configurable: true,
        value: { writeText: (value) => { window.__copiedDebugReport = value; return Promise.resolve(); } },
      });
      document.querySelector('#copy-debug').click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__copiedDebugReport?.includes('Synthetic browser failure')"),
      "copyable Debug report",
    );
    assert.equal(
      await evaluate(cdp, appSession, `(() => {
        const tab = document.querySelector('#debug-button');
        tab.focus();
        tab.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowLeft', bubbles: true, cancelable: true }));
        return document.activeElement.id;
      })()`),
      "ai-mode-button",
      "arrow keys must move between and activate studio tabs",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        document.querySelector('.skip-link').click();
        return {
          activeTab: document.querySelector('[role="tab"][aria-selected="true"]').id,
          focused: document.activeElement.id,
          aiHidden: document.querySelector('#ai-mode-panel').hidden,
        };
      })()`),
      { activeTab: "ai-mode-button", focused: "timeline-panel", aiHidden: false },
      "the skip link must reveal and focus the timeline from another tab",
    );
    const durationBaseline = await evaluate(cdp, appSession, `(() => ({
      duration: Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')),
      button: document.querySelector('#ai-duration-button')?.textContent.trim(),
    }))()`);
    assert.equal(durationBaseline.button, "Duration");
    await evaluate(cdp, appSession, `(() => {
      window.prompt = () => null;
      document.querySelector('#ai-duration-button').click();
    })()`);
    assert.equal(
      await evaluate(cdp, appSession, "Number(document.querySelector('.track-lane').getAttribute('aria-valuemax'))"),
      durationBaseline.duration,
      "cancelling the AI Mode duration prompt must preserve the project",
    );
    await evaluate(cdp, appSession, `(() => {
      window.prompt = () => '301';
      document.querySelector('#ai-duration-button').click();
    })()`);
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => ({
        duration: Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')),
        toast: document.querySelector('#toast-message').textContent,
        error: document.querySelector('#toast').classList.contains('is-error'),
      }))()`),
      {
        duration: durationBaseline.duration,
        toast: "Enter a duration between 1 second and 5 minutes",
        error: true,
      },
      "invalid duration input must be rejected in the browser",
    );
    const resizedDuration = durationBaseline.duration === 300 ? 299 : durationBaseline.duration + 1;
    await evaluate(cdp, appSession, `(() => {
      window.prompt = () => '${resizedDuration}';
      document.querySelector('#ai-duration-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')) === ${resizedDuration}`,
      ),
      "AI Mode duration resize",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#total-time').textContent"),
      `/ ${Math.floor(resizedDuration / 60)}:${String(resizedDuration % 60).padStart(2, "0")}`,
    );
    await evaluate(cdp, appSession, `(() => {
      window.prompt = () => '${durationBaseline.duration}';
      document.querySelector('#ai-duration-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `fetch('/api/project').then((response) => response.json()).then(
          (project) => project.duration === ${durationBaseline.duration}
        )`,
      ),
      "duration restore",
    );
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')) === ${durationBaseline.duration}`,
      ),
      "restored timeline duration",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').disabled"),
      "spectrum readiness after duration restore",
      60_000,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, `(() => {
        const [minutes, seconds] = document.querySelector('#current-time').textContent.split(':');
        return Number(minutes) * 60 + Number(seconds) > 1.2;
      })()`),
      "playhead beyond shortened duration",
    );
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#play-button').click();
      window.prompt = () => '1';
      document.querySelector('#ai-duration-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelector('#current-time').textContent === '0:01.0' &&
          Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')) === 1`,
      ),
      "playhead clamped to shortened duration",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')) === ${durationBaseline.duration}`,
      ),
      "shortened duration undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#debug-button').click()");
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelectorAll('.track-spectrum i').length"),
      24,
      "each demo track must have an eight-band spectrum analyzer",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#audio-metrics-toggle')"),
      null,
      "Debug must not expose an audio metrics toggle",
    );
    await evaluate(cdp, appSession, "document.querySelector('#ai-mode-button').click()");

    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 390, height: 844, deviceScaleFactor: 1, mobile: true },
      appSession,
    );
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 1 }, appSession);
    await evaluate(cdp, appSession, "document.querySelector('#timeline-scroll').scrollLeft = 0");
    const mobileLane = await evaluate(cdp, appSession, `(() => {
      const rect = document.querySelector('.track-lane').getBoundingClientRect();
      return { y: rect.top + rect.height / 2 };
    })()`);
    const selectionBeforePan = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#selection-readout').textContent",
    );
    await evaluate(cdp, appSession, "window.scrollTo(0, 0)");
    await touch(cdp, appSession, "touchStart", [{ x: 250, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 250, y: mobileLane.y - 110 }]);
    await touch(cdp, appSession, "touchMove", [{ x: 250, y: mobileLane.y - 230 }]);
    await touch(cdp, appSession, "touchEnd", []);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.scrollY > 100"),
      "native vertical page panning over a timeline lane",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      selectionBeforePan,
      "vertical panning must not rewrite the selection",
    );
    await evaluate(cdp, appSession, "window.scrollTo(0, 0)");
    await waitFor(async () => evaluate(cdp, appSession, "window.scrollY === 0"), "page scroll reset");
    await touch(cdp, appSession, "touchStart", [{ x: 340, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 250, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 140, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchEnd", []);
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#timeline-scroll').scrollLeft > 100"),
      "native mobile timeline panning",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      selectionBeforePan,
      "panning must not rewrite the selection",
    );

    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#timeline-scroll').scrollLeft = 0;
      document.querySelector('#selection-mode-button').click();
    })()`);
    await touch(cdp, appSession, "touchStart", [{ x: 180, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchMove", [{ x: 300, y: mobileLane.y }]);
    await touch(cdp, appSession, "touchEnd", []);
    assert.notEqual(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      selectionBeforePan,
      "explicit mobile selection mode must edit the region",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-mode-button').getAttribute('aria-pressed')"),
      "false",
      "mobile selection mode must return gesture ownership to panning",
    );
    const wholeTrackSelection = await evaluate(cdp, appSession, `(() => {
      const duration = Number(document.querySelector('.track-lane').getAttribute('aria-valuemax'));
      return \`0.0s - \${duration.toFixed(1)}s\`;
    })()`);

    await touch(cdp, appSession, "touchStart", [{ x: 240, y: mobileLane.y }]);
    await new Promise((resolve) => setTimeout(resolve, 600));
    await touch(cdp, appSession, "touchEnd", []);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      wholeTrackSelection,
      "long-pressing a mobile timeline lane must select the whole track",
    );
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: false }, appSession);
    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 1440, height: 900, deviceScaleFactor: 1, mobile: false },
      appSession,
    );
    const doubleClickLane = await evaluate(cdp, appSession, `(() => {
      const rect = document.querySelector('.track-lane').getBoundingClientRect();
      return { x: rect.left + rect.width * 0.6, y: rect.top + rect.height / 2 };
    })()`);
    await mouse(cdp, appSession, "mousePressed", doubleClickLane.x, doubleClickLane.y, 1, 1);
    await mouse(cdp, appSession, "mouseReleased", doubleClickLane.x, doubleClickLane.y, 0, 1);
    await mouse(cdp, appSession, "mousePressed", doubleClickLane.x, doubleClickLane.y, 1, 2);
    await mouse(cdp, appSession, "mouseReleased", doubleClickLane.x, doubleClickLane.y, 0, 2);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      wholeTrackSelection,
      "double-clicking a desktop timeline lane must select the whole track",
    );
    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 1600, height: 1000, deviceScaleFactor: 1, mobile: false },
      appSession,
    );

    const lane = await evaluate(cdp, appSession, `(() => {
      const rect = document.querySelector('.track-lane').getBoundingClientRect();
      return { left: rect.left, right: rect.right, y: rect.top + rect.height / 2, width: rect.width };
    })()`);
    await mouse(cdp, appSession, "mousePressed", lane.right - 1, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.right - 1, lane.y);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "31.8s - 32.0s",
      "right-edge click must retain a valid selection",
    );
    await mouse(cdp, appSession, "mousePressed", lane.right - 1, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.75, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.75, lane.y);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "24.0s - 32.0s",
      "a backward drag must preserve the true right-edge anchor",
    );

    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.0s - 16.0s",
    );
    await evaluate(cdp, appSession, "document.querySelector('.track-lane').focus()");
    await pressKey(cdp, appSession, "ArrowRight", "ArrowRight", 39);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.3s - 16.3s",
      "keyboard arrows must move the selected region",
    );
    await pressKey(cdp, appSession, "ArrowLeft", "ArrowLeft", 37, 8);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.3s - 16.0s",
      "Shift plus Arrow must resize the selected region",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.classList.contains('track-lane')"),
      true,
      "keyboard selection must retain timeline focus",
    );
    await mouse(cdp, appSession, "mousePressed", lane.left + lane.width * 0.25, lane.y, 1);
    await mouse(cdp, appSession, "mouseMoved", lane.left + lane.width * 0.5, lane.y, 1);
    await mouse(cdp, appSession, "mouseReleased", lane.left + lane.width * 0.5, lane.y);

    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__refusedEditRequests = 0;
      window.__refusedEditBody = null;
      window.__persistedBatchAtRequest = null;
      window.__persistedSlimAtRequest = null;
      window.__persistedDynamicAtRequest = null;
      window.fetch = function fetch(resource, options) {
        if (resource === '/api/edits') {
          window.__refusedEditRequests += 1;
          window.__refusedEditBody = options.body.toString();
          const pending = JSON.parse(localStorage.getItem('daw-ai.pending-edit.v1'));
          window.__persistedBatchAtRequest = pending.batchParameterTools;
          window.__persistedSlimAtRequest = pending.slimPrompt;
          window.__persistedDynamicAtRequest = pending.dynamicTools;
          return Promise.resolve(new Response(JSON.stringify({ error: 'Edit request refused' }), {
            status: 422,
            headers: { 'Content-Type': 'application/json' },
          }));
        }
        return originalFetch(resource, options);
      };
      window.__restoreFetchAfterRefusedEdit = () => {
        window.fetch = originalFetch;
      };
      document.querySelector('#slim-prompt').click();
      document.querySelector('#dynamic-tools').click();
      const input = document.querySelector('#prompt-input');
      input.value = 'refused edit';
      document.querySelector('#prompt-form').requestSubmit();
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(
          cdp,
          appSession,
          `!document.querySelector('#compose-button').disabled &&
          document.querySelector('#toast').classList.contains('is-error') &&
          document.querySelector('#toast-message').textContent === 'Edit request refused'`,
      ),
      "definitive edit-acceptance refusal",
    );
    const refusedEditRequests = await evaluate(cdp, appSession, "window.__refusedEditRequests");
    assert.equal(
      refusedEditRequests,
      1,
      `an explicit acceptance refusal must not retry for the edit execution window (saw ${refusedEditRequests})`,
    );
    const refusedEditBody = await evaluate(
      cdp,
      appSession,
      "Object.fromEntries(new URLSearchParams(window.__refusedEditBody))",
    );
    assert.equal(
      refusedEditBody.batch_parameter_tools,
      "true",
      "the selected batch-tool variant must travel with the edit request",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__persistedBatchAtRequest"),
      true,
      "the variant must be retained with an unaccepted recoverable edit",
    );
    assert.equal(
      refusedEditBody.slim_prompt,
      "true",
      "the selected slim-prompt variant must travel with the edit request",
    );
    assert.equal(
      refusedEditBody.dynamic_tools,
      "true",
      "the selected dynamic-tools variant must travel independently",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__persistedSlimAtRequest"),
      true,
      "the slim-prompt variant must be retained with an unaccepted edit",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__persistedDynamicAtRequest"),
      true,
      "the dynamic-tools variant must be retained with an unaccepted edit",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#reference-audio')"),
      null,
      "the unused reference-audio upload must be absent",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterRefusedEdit();
      document.querySelector('#prompt-input').value = '';
    })()`);
    await evaluate(cdp, appSession, `(() => {
      const originalPlay = HTMLMediaElement.prototype.play;
      window.__transportMedia = null;
      window.__transportPlayCalls = [];
      window.__startPlaybackFrameSample = () => {
        window.__playbackFrames = [];
        const samplePlaybackFrame = (timestamp) => {
          window.__playbackFrames.push(timestamp);
          if (window.__playbackFrames.length < 180) requestAnimationFrame(samplePlaybackFrame);
        };
        requestAnimationFrame(samplePlaybackFrame);
      };
      HTMLMediaElement.prototype.play = function play(...args) {
        if (window.__transportMedia === null) window.__transportMedia = this;
        window.__transportPlayCalls.push({
          activeUserGesture: navigator.userActivation.isActive,
          sameElement: window.__transportMedia === this,
          source: this.getAttribute('src'),
        });
        return originalPlay.apply(this, args);
      };
      window.__restoreMediaPlay = () => {
        HTMLMediaElement.prototype.play = originalPlay;
      };
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () => {
        const playback = await evaluate(cdp, appSession, `({
          state: document.documentElement.dataset.audioState,
          error: document.querySelector('#toast').classList.contains('is-error')
            ? document.querySelector('#toast-message').textContent
            : null,
        })`);
        if (playback.state === "idle" && playback.error) throw new Error(playback.error);
        return playback.state === "playing";
      },
      "playback before prompted edit",
      30_000,
    );
    const initialPlayCall = await evaluate(
      cdp,
      appSession,
      "window.__transportPlayCalls[0]",
    );
    assert.equal(initialPlayCall.activeUserGesture, true, "initial media.play() must retain the Play-button gesture");
    assert.equal(initialPlayCall.sameElement, true);
    assert.match(initialPlayCall.source, /^\/api\/audio-stream\//);
    const initialPromptPlaybackTime = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#current-time').textContent",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#current-time').textContent !== ${JSON.stringify(initialPromptPlaybackTime)}`,
        ),
      "transport movement before prompted edit",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          "document.querySelector('#track-rows').dataset.spectrumCoverage !== undefined",
        ),
      "backend spectrum timeline",
      60_000,
    ).catch(async (error) => {
      const diagnostics = await evaluate(cdp, appSession, `({
        audioState: document.documentElement.dataset.audioState,
        displayedTime: document.querySelector('#current-time').textContent,
        spectrumCoverage: document.querySelector('#track-rows').dataset.spectrumCoverage,
        spectrumFrame: document.querySelector('#track-rows').dataset.spectrumFrame,
        requests: performance.getEntriesByType('resource')
          .filter((entry) => entry.name.includes('/api/track-spectrum/')).map((entry) => entry.name),
        issues: document.querySelector('#debug-report').value,
      })`);
      throw new Error(`${error.message}: ${JSON.stringify(diagnostics)}`);
    });
    await evaluate(cdp, appSession, `(() => {
      if (document.documentElement.dataset.audioState === 'idle') {
        document.querySelector('#play-button').click();
      }
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        "document.documentElement.dataset.audioState === 'playing'",
      ),
      "active playback with loaded spectrum",
      30_000,
    );
    await evaluate(cdp, appSession, "window.__startPlaybackFrameSample()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `[...document.querySelectorAll('.track-spectrum i')].some(
            (bar) => Number.parseFloat(bar.style.getPropertyValue('--spectrum-level')) > 0.01
          )`,
        ),
      "track response to backend spectrum timeline",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__playbackFrames.length >= 60"),
      "playback frame timing sample",
    );
    const playbackVisualTiming = await evaluate(cdp, appSession, `(() => ({
      maximumFrameGap: Math.max(...window.__playbackFrames.slice(1).map(
        (timestamp, index) => timestamp - window.__playbackFrames[index]
      )),
      maximumSpectrumLevel: Math.max(...[...document.querySelectorAll('.track-spectrum i')].map(
        (bar) => Number.parseFloat(bar.style.getPropertyValue('--spectrum-level')) || 0
      )),
    }))()`);
    assert.ok(
      playbackVisualTiming.maximumFrameGap < 100,
      `playback visuals must not block the UI thread (${playbackVisualTiming.maximumFrameGap} ms gap)`,
    );
    assert.ok(
      playbackVisualTiming.maximumSpectrumLevel > 0.1,
      `spectrum magnitude must remain legible (${playbackVisualTiming.maximumSpectrumLevel})`,
    );
    await evaluate(cdp, appSession, "new Promise((resolve) => setTimeout(resolve, 1500))");
    await evaluate(cdp, appSession, `(() => {
      if (document.documentElement.dataset.audioState === 'idle') {
        document.querySelector('#play-button').click();
      }
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        "document.documentElement.dataset.audioState === 'playing'",
      ),
      "active playback after cold spectrum preparation",
      30_000,
    );
    for (const handoffTime of [15, 28]) {
      const handoffRequestBaseline = await evaluate(cdp, appSession, "performance.now()");
      await evaluate(cdp, appSession, `(() => {
        window.__handoffFrames = [];
        window.__handoffSpectrumStart = Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0);
        const sample = (timestamp) => {
          window.__handoffFrames.push(timestamp);
          if (window.__handoffFrames.length < 60) requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
        const lane = document.querySelector('.track-lane');
        lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }));
        for (let index = 0; index < ${handoffTime * 4}; index += 1) {
          lane.dispatchEvent(new KeyboardEvent('keydown', {
            key: 'ArrowRight', bubbles: true, cancelable: true,
          }));
        }
      })()`);
      await waitFor(
        async () => evaluate(
          cdp,
          appSession,
          `document.documentElement.dataset.audioState === 'playing' &&
            document.querySelector('#current-time').textContent.startsWith('${Math.floor(handoffTime / 60)}:${String(handoffTime % 60).padStart(2, "0")}') &&
            window.__handoffFrames.length >= 60 &&
            Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0) >=
              window.__handoffSpectrumStart + 4`,
        ),
        `continuous analyzer spectrum handoff at ${handoffTime} seconds`,
        30_000,
      ).catch(async (error) => {
        const diagnostics = await evaluate(cdp, appSession, `({
          audioState: document.documentElement.dataset.audioState,
          mediaTime: window.__transportMedia.currentTime,
          displayedTime: document.querySelector('#current-time').textContent,
          spectrumFrame: document.querySelector('#track-rows').dataset.spectrumFrame,
          spectrumStart: window.__handoffSpectrumStart,
          spectrumCoverage: document.querySelector('#track-rows').dataset.spectrumCoverage,
          animationFrames: window.__handoffFrames.length,
          issues: document.querySelector('#debug-report').value,
        })`);
        throw new Error(`${error.message}: ${JSON.stringify(diagnostics)}`);
      });
      const handoff = await evaluate(cdp, appSession, `({
        spectrumFrames: Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0) -
          window.__handoffSpectrumStart,
        maximumFrameGap: Math.max(...window.__handoffFrames.slice(1).map(
          (timestamp, index) => timestamp - window.__handoffFrames[index]
        )),
      })`);
      assert.ok(handoff.spectrumFrames >= 4, `analyzer must keep updating through handoff (${handoff.spectrumFrames})`);
      assert.ok(handoff.maximumFrameGap < 100, `handoff must not block UI (${handoff.maximumFrameGap} ms)`);
      const newSpectrumRequests = await evaluate(
          cdp,
          appSession,
          `performance.getEntriesByType('resource').filter(
            (entry) => entry.name.includes('/api/track-spectrum/') &&
              entry.startTime >= ${handoffRequestBaseline}
          ).map((entry) => entry.name)`,
        );
      assert.equal(
        newSpectrumRequests.length,
        0,
        `timeline traversal must not request another spectrum render (${JSON.stringify(newSpectrumRequests)})`,
      );
      assert.ok(
        await evaluate(
          cdp,
          appSession,
          "Number(document.querySelector('#track-rows').dataset.spectrumLagMs) <= 75",
        ),
        "displayed spectrum frames must stay synchronized to the audible media clock",
      );
    }
    const rewindCacheBaseline = await evaluate(cdp, appSession, `({
      requestBaseline: performance.now(),
      spectrumFrames: Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0),
    })`);
    await evaluate(cdp, appSession, "document.querySelector('#rewind-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.documentElement.dataset.audioState === 'playing' &&
          Number(document.querySelector('#current-time').textContent.slice(2)) < 2 &&
          Number(document.querySelector('#track-rows').dataset.spectrumFrame || 0) >=
            ${rewindCacheBaseline.spectrumFrames + 4}`,
      ),
      "cached analyzer playback after active rewind",
      10_000,
    ).catch(async (error) => {
      const diagnostics = await evaluate(cdp, appSession, `({
        audioState: document.documentElement.dataset.audioState,
        mediaTime: window.__transportMedia.currentTime,
        mediaSource: window.__transportMedia.getAttribute('src'),
        displayedTime: document.querySelector('#current-time').textContent,
        spectrumFrame: document.querySelector('#track-rows').dataset.spectrumFrame,
        spectrumBaseline: ${rewindCacheBaseline.spectrumFrames},
        spectrumCoverage: document.querySelector('#track-rows').dataset.spectrumCoverage,
        requests: performance.getEntriesByType('resource')
          .filter((entry) => entry.name.includes('/api/track-spectrum/')).map((entry) => entry.name),
      })`);
      throw new Error(`${error.message}: ${JSON.stringify(diagnostics)}`);
    });
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        `performance.getEntriesByType('resource').filter(
          (entry) => entry.name.includes('/api/track-spectrum/') &&
            entry.startTime >= ${rewindCacheBaseline.requestBaseline}
        ).length`,
      ),
      0,
      "rewind must reuse the cached opening spectrum without another render",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        `performance.getEntriesByType('resource').some(
          (entry) => entry.name.includes('/api/track-spectrum/') && entry.name.endsWith('/0')
        )`,
      ),
      true,
      "playback must cache its opening spectrum window",
    );
    const seekDebouncePlayCalls = await evaluate(
      cdp,
      appSession,
      "window.__transportPlayCalls.length",
    );
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('#rewind-button').click();
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.documentElement.dataset.audioState === 'playing' &&
          window.__transportPlayCalls.length > ${seekDebouncePlayCalls}`,
      ),
      "Play starts immediately during a pending seek debounce",
      10_000,
    );
    const promptSingleFlight = await evaluate(cdp, appSession, `(async () => {
      const originalFetch = window.fetch;
      const deferred = [];
      let promptRequestsReleased = false;
      window.__promptRequestCount = 0;
      window.__promptOperationIds = [];
      window.__editPollCount = 0;
      window.fetch = function fetch(resource, options) {
        if (typeof resource === 'string' && resource.startsWith('/api/edits/')) {
          window.__editPollCount += 1;
          if (window.__editPollCount === 1) {
            return Promise.resolve(new Response(JSON.stringify({
              id: resource.split('/').at(-1),
              operationId: window.__acceptedOperationId,
              status: 'running',
              phase: 'planning',
              detail: 'Gemini is arranging the requested change',
              elapsedSeconds: 73,
              timeoutSeconds: 1200,
              pollAfterMs: 100,
            }), {
              status: 200,
              headers: { 'Content-Type': 'application/json' },
            }));
          }
          if (window.__editPollCount === 2) {
            return Promise.resolve(new Response('not valid JSON', {
              status: 200,
              headers: { 'Content-Type': 'application/json' },
            }));
          }
          if (window.__editPollCount === 3) {
            return new Promise((_resolve, reject) => {
              options.signal.addEventListener('abort', () => {
                reject(new DOMException('Simulated hanging edit-status request', 'AbortError'));
              }, { once: true });
            });
          }
          if (window.__editPollCount <= 7) {
            return new Promise((_resolve, reject) => window.setTimeout(
              () => reject(new TypeError('Simulated transient edit-status failure')),
              50,
            ));
          }
          if (window.__editPollCount === 8) {
            return Promise.resolve(new Response('<h1>Not Found</h1>', {
              status: 404,
              headers: { 'Content-Type': 'text/html' },
            }));
          }
          return originalFetch(resource, options);
        }
        if (resource !== '/api/edits') return originalFetch(resource, options);
        window.__promptRequestCount += 1;
        window.__promptOperationIds.push(new URLSearchParams(options.body).get('operation_id'));
        if (promptRequestsReleased) {
          return originalFetch(resource, options).then(async (response) => {
            if (window.__promptRequestCount === 2) {
              return new Response(JSON.stringify({
                error: 'Simulated gateway timeout after forwarding',
              }), {
                status: 504,
                headers: { 'Content-Type': 'application/json' },
              });
            }
            const job = await response.clone().json();
            return new Response(JSON.stringify({
              ...job,
              status: 'queued',
              phase: 'queued',
              detail: 'Waiting for the edit worker',
              pollAfterMs: 20,
            }), {
              status: 202,
              headers: { 'Content-Type': 'application/json' },
            });
          });
        }
        return new Promise((resolve, reject) => deferred.push({ resource, options, resolve, reject }));
      };
      window.__releasePromptRequests = () => {
        promptRequestsReleased = true;
        for (const request of deferred) {
          originalFetch(request.resource, request.options).then(async (response) => {
            window.__acceptedOperationId = (await response.clone().json()).operationId;
            request.resolve(new Response('not valid JSON', {
              status: 202,
              headers: { 'Content-Type': 'application/json' },
            }));
          }, request.reject);
        }
      };
      window.__restorePromptFetch = () => {
        window.fetch = originalFetch;
      };
      const input = document.querySelector('#prompt-input');
      input.value = 'increase volume';
      input.dispatchEvent(new KeyboardEvent('keydown', {
        key: 'Enter', code: 'Enter', ctrlKey: true, bubbles: true, cancelable: true,
      }));
      input.dispatchEvent(new KeyboardEvent('keydown', {
        key: 'Enter', code: 'Enter', metaKey: true, bubbles: true, cancelable: true,
      }));
      await Promise.resolve();
      return {
        requests: window.__promptRequestCount,
        submitDisabled: document.querySelector('#compose-button').disabled,
        transportActive: document.querySelector('#play-button').classList.contains('is-playing'),
        progressVisible: !document.querySelector('#edit-progress').hidden,
        progressText: document.querySelector('#edit-progress-label').textContent,
      };
    })()`);
    assert.deepEqual(
      promptSingleFlight,
      {
        requests: 1,
        submitDisabled: false,
        transportActive: false,
        progressVisible: true,
        progressText: "Starting the AI edit",
      },
      "prompt shortcuts must share one in-flight edit request",
    );
    await evaluate(cdp, appSession, "document.querySelector('#prompt-input').value = 'draft the next edit'");
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#play-button').classList.contains('is-playing')"),
      "playback started while prompt is pending",
    );
    await evaluate(cdp, appSession, "window.__releasePromptRequests()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#edit-progress-label').textContent === 'Gemini is arranging the requested change' &&
            document.querySelector('#edit-progress-time').textContent === '1:13 / 20:00' &&
            document.querySelector('#edit-progress-fill').style.width === '14%' &&
            document.querySelector('#edit-progress-track').getAttribute('aria-valuenow') === null`,
        ),
      "running Gemini progress",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__editPollCount >= 7"),
      "malformed and transient edit-status failures",
      12_000,
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        submitDisabled: document.querySelector('#compose-button').disabled,
        progressVisible: !document.querySelector('#edit-progress').hidden,
        progressText: document.querySelector('#edit-progress-label').textContent,
        renderedEdits: Number(document.querySelector('#session-history-list').dataset.currentEditCount),
      })`),
      {
        submitDisabled: false,
        progressVisible: true,
        progressText: "Connection interrupted; still waiting for the accepted edit",
        renderedEdits: 0,
      },
      "poll failures must leave the accepted edit pending until status reconciliation",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, 'Number(document.querySelector(\'#session-history-list\').dataset.currentEditCount) === 1'),
      "single-flight prompt reconciliation after status loss",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#compose-button').disabled"),
      "prompt submission lock release",
      30_000,
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__promptRequestCount"),
      3,
      "malformed and gateway acceptance responses must retry with the same operation",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, "window.__promptOperationIds"),
      [
        await evaluate(cdp, appSession, "window.__acceptedOperationId"),
        await evaluate(cdp, appSession, "window.__acceptedOperationId"),
        await evaluate(cdp, appSession, "window.__acceptedOperationId"),
      ],
      "acceptance retries must preserve the client-generated operation ID",
    );
    const reconciledPrompt = await evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      return {
        serverVersion: project.version,
        serverEdits: project.edits.length,
        renderedEdits: Number(document.querySelector('#session-history-list').dataset.currentEditCount),
        savedState: document.querySelector('#saved-state').textContent,
        errorToast: !document.querySelector('#toast').hidden &&
          document.querySelector('#toast').classList.contains('is-error'),
        toastText: document.querySelector('#toast-message').textContent,
      };
    })()`);
    assert.equal(reconciledPrompt.serverEdits, 1);
    assert.equal(reconciledPrompt.renderedEdits, reconciledPrompt.serverEdits);
    assert.equal(reconciledPrompt.savedState, `Version ${reconciledPrompt.serverVersion}`);
    assert.equal(reconciledPrompt.errorToast, false, reconciledPrompt.toastText);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelectorAll('#session-history-list [data-history-index]').length >= 2`,
      ),
      "session history after completed edit",
    );
    assert.equal(
      await evaluate(cdp, appSession, `(() => {
        const indexes = [...document.querySelectorAll('#session-history-list [data-history-index]')]
          .map((item) => Number(item.dataset.historyIndex));
        return indexes.every((index, position) => position === 0 || indexes[position - 1] > index);
      })()`),
      true,
      "session history must display the most recent state first",
    );
    const historyIndexes = await evaluate(cdp, appSession, `(() => {
      const items = [...document.querySelectorAll('#session-history-list [data-history-index]')];
      return { newest: items[0].dataset.historyIndex, oldest: items.at(-1).dataset.historyIndex };
    })()`);
    await evaluate(
      cdp,
      appSession,
      `document.querySelector('#session-history-list [data-history-index="${historyIndexes.oldest}"]').click()`,
    );
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelector('#session-history-list [aria-current="step"]')?.dataset.historyIndex === '${historyIndexes.oldest}'`,
      ),
      "backward session history navigation",
    );
    await evaluate(
      cdp,
      appSession,
      `document.querySelector('#session-history-list [data-history-index="${historyIndexes.newest}"]').click()`,
    );
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelector('#session-history-list [aria-current="step"]')?.dataset.historyIndex === '${historyIndexes.newest}'`,
      ),
      "forward session history navigation",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#compose-button').disabled"),
      false,
      "prompt submission must release its lock after completion",
    );
    assert.ok(
      await evaluate(cdp, appSession, "window.__editPollCount >= 8"),
      "prompt submission must reconcile its asynchronous edit after transient failures and terminal status loss",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#edit-progress').hidden"),
      true,
      "prompt progress must hide after completion",
    );
    await evaluate(cdp, appSession, "window.__restorePromptFetch()");
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#prompt-input').value"),
      "draft the next edit",
      "a successful request must preserve prompt text drafted while it was pending",
    );
    const promptedEditResumeTime = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#current-time').textContent",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#play-button').classList.contains('is-playing') &&
            document.querySelector('#current-time').textContent !== ${JSON.stringify(promptedEditResumeTime)}`,
      ),
      "playback restoration after prompted edit",
      30_000,
    );
    const compoundPlaybackTime = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#current-time').textContent",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__projectRefreshFailures = 0;
      window.fetch = function fetch(resource, options) {
        if (resource === '/api/project' && window.__projectRefreshFailures === 0) {
          window.__projectRefreshFailures += 1;
          return new Promise((_resolve, reject) => {
            options.signal.addEventListener('abort', () => {
              reject(new DOMException('Simulated hanging project refresh', 'AbortError'));
            }, { once: true });
          });
        }
        return originalFetch(resource, options);
      };
      window.__restoreFetchAfterProjectRefresh = () => {
        window.fetch = originalFetch;
      };
      const input = document.querySelector('#prompt-input');
      input.value = 'make the chords warm and spacious';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, `Number(document.querySelector('#session-history-list').dataset.currentEditCount) === 2`),
      "compound AI edit after project refresh retry",
    );
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelector('#play-button').classList.contains('is-playing') &&
          document.querySelector('#current-time').textContent !== ${JSON.stringify(compoundPlaybackTime)}`,
      ),
      "pre-submit playback restoration without a manual restart",
      30_000,
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__projectRefreshFailures"),
      1,
      "a committed edit must retry project synchronization separately",
    );
    await evaluate(cdp, appSession, "window.__restoreFetchAfterProjectRefresh()");
    const compoundProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.equal(compoundProject.tracks.length, 3, "effect prompt must not add a track");
    const compoundEdit = compoundProject.edits[compoundProject.edits.length - 1];
    assert.equal(compoundEdit.action.type, "compound");
    assert.deepEqual(
      compoundEdit.action.actions.map((action) => action.type),
      ["effect", "filter"],
    );
    const projectBeforeConflict = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    const conflictDuration =
      projectBeforeConflict.duration === 300 ? 299 : projectBeforeConflict.duration + 1;
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__conflictProjectRefreshes = 0;
      window.__conflictPollCount = 0;
      window.__durationDuringPromptRequests = 0;
      window.__releaseConflictStatus = false;
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/edits') {
          window.__conflictOperationId = new URLSearchParams(options.body).get('operation_id');
          return new Response(JSON.stringify({
            id: 'conflict-test', operationId: window.__conflictOperationId, status: 'queued', phase: 'queued',
            detail: 'Waiting for the edit worker', elapsedSeconds: 0,
            timeoutSeconds: 1200, pollAfterMs: 20,
          }), { status: 202, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/edits/conflict-test') {
          window.__conflictPollCount += 1;
          if (!window.__releaseConflictStatus) {
            return new Response(JSON.stringify({
              id: 'conflict-test', operationId: window.__conflictOperationId, status: 'running', phase: 'planning',
              detail: 'Gemini is planning the edit', pollAfterMs: 20,
              elapsedSeconds: 1, timeoutSeconds: 1200,
            }), { status: 200, headers: { 'Content-Type': 'application/json' } });
          }
          return new Response(JSON.stringify({
            id: 'conflict-test', operationId: window.__conflictOperationId, status: 'failed', phase: 'failed',
            errorStatus: 409, error: 'the project changed; submit the edit again',
            elapsedSeconds: 1, timeoutSeconds: 1200,
          }), { status: 200, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/duration') window.__durationDuringPromptRequests += 1;
        if (resource === '/api/project') window.__conflictProjectRefreshes += 1;
        return originalFetch(resource, options);
      };
      window.__restoreFetchAfterConflict = () => {
        window.fetch = originalFetch;
      };
      const input = document.querySelector('#prompt-input');
      input.value = 'conflicting prompt';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__conflictPollCount >= 1"),
      "accepted edit polling before a manual mutation",
    );
    await evaluate(cdp, appSession, `(() => {
      window.prompt = () => '${conflictDuration}';
      document.querySelector('#ai-duration-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `window.__durationDuringPromptRequests === 1 &&
          Number(document.querySelector('.track-lane').getAttribute('aria-valuemax')) === ${conflictDuration} &&
          !document.querySelector('#compose-button').disabled`,
      ),
      "queued Duration mutation during accepted edit polling",
    );
    await evaluate(cdp, appSession, "window.__releaseConflictStatus = true");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `!document.querySelector('#compose-button').disabled &&
          document.querySelector('#toast-message').textContent === 'the project changed; submit the edit again'`,
      ),
      "conflicted edit project reconciliation",
    );
    const conflictProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.ok(await evaluate(cdp, appSession, "window.__conflictProjectRefreshes >= 2"));
    assert.equal(conflictProject.version, projectBeforeConflict.version + 1);
    assert.equal(conflictProject.duration, conflictDuration);
    assert.equal(
      await evaluate(cdp, appSession, "window.__durationDuringPromptRequests"),
      1,
      "accepted edit polling must not own the project mutation queue",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#saved-state').textContent"),
      `Version ${conflictProject.version}`,
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterConflict();
      document.querySelector('#prompt-input').value = '';
      document.querySelector('#undo-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `fetch('/api/project').then((response) => response.json()).then(
          (project) => project.duration === ${projectBeforeConflict.duration}
        )`,
      ),
      "conflict test project restoration",
    );

    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      const deferred = [];
      window.__undoRequestCount = 0;
      window.__undoHadAbortSignal = [];
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/undo') return originalFetch(resource, options);
        window.__undoRequestCount += 1;
        window.__undoHadAbortSignal.push(Boolean(options.signal));
        return new Promise((resolve, reject) => deferred.push({ resource, options, resolve, reject }));
      };
      window.__releaseNextUndoRequest = () => {
        const request = deferred.shift();
        if (!request) return false;
        originalFetch(request.resource, request.options).then(request.resolve, request.reject);
        return true;
      };
      window.__restoreFetchAfterUndo = () => {
        window.fetch = originalFetch;
      };
      const button = document.querySelector('#undo-button');
      button.click();
      button.click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__undoRequestCount === 1"),
      "first serialized undo request",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__undoRequestCount"),
      1,
      "a second undo must wait for the first project snapshot",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, "window.__undoHadAbortSignal"),
      [false],
      "non-idempotent undo must not be abandoned on a client timeout",
    );
    await evaluate(cdp, appSession, "window.__releaseNextUndoRequest()");
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__undoRequestCount === 2"),
      "second serialized undo request",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, "window.__undoHadAbortSignal"),
      [false, false],
      "serialized undo requests must await their authoritative response",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterUndo();
      window.__releaseNextUndoRequest();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, `(async () => {
        const project = await fetch('/api/project').then((response) => response.json());
        const currentHistory = document.querySelector('#session-history-list [aria-current="step"]');
        return project.edits.length === 0 &&
          document.querySelector('#saved-state').textContent === 'Version ' + project.version &&
          Number(currentHistory?.dataset.historyVersion) === project.version;
      })()`),
      "serialized undo completion",
    );

    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      let competingEditCreated = false;
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/edits') {
          window.__missingOperationId = new URLSearchParams(options.body).get('operation_id');
          return new Response(JSON.stringify({
            id: 'missing-job', operationId: window.__missingOperationId, status: 'queued', phase: 'queued',
            detail: 'Waiting for the edit worker', elapsedSeconds: 0,
            timeoutSeconds: 1200, pollAfterMs: 20,
          }), { status: 202, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/edits/missing-job') {
          if (!competingEditCreated) {
            competingEditCreated = true;
            const accepted = await originalFetch('/api/edits', {
              method: 'POST',
              headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
              body: new URLSearchParams({
                prompt: 'increase volume',
                start: '4',
                end: '8',
              }),
            }).then((response) => response.json());
            window.__competingOperationId = accepted.operationId;
            for (;;) {
              const status = await originalFetch('/api/edits/' + accepted.id).then((response) => response.json());
              if (status.status === 'completed' || status.status === 'failed') break;
              await new Promise((resolve) => window.setTimeout(resolve, 20));
            }
          }
          return new Response(JSON.stringify({ error: 'edit job not found' }), {
            status: 404,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        return originalFetch(resource, options);
      };
      window.__restoreFetchAfterOperationIdentity = () => {
        window.fetch = originalFetch;
      };
      const input = document.querySelector('#prompt-input');
      input.value = 'increase volume';
      document.querySelector('#prompt-form').requestSubmit();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `!document.querySelector('#compose-button').disabled &&
          document.querySelector('#toast').classList.contains('is-error') &&
          document.querySelector('#toast-message').textContent.startsWith('The edit status was lost')`,
      ),
      "operation-bound status-loss reconciliation",
    );
    const operationIdentity = await evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      return {
        competingOperationId: window.__competingOperationId,
        renderedEdits: Number(document.querySelector('#session-history-list').dataset.currentEditCount),
        prompt: document.querySelector('#prompt-input').value,
        projectOperationId: project.edits.at(-1).operationId,
        missingOperationId: window.__missingOperationId,
      };
    })()`);
    assert.equal(operationIdentity.renderedEdits, 1);
    assert.equal(operationIdentity.prompt, "increase volume");
    assert.equal(operationIdentity.projectOperationId, operationIdentity.competingOperationId);
    assert.notEqual(operationIdentity.projectOperationId, operationIdentity.missingOperationId);
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterOperationIdentity();
      document.querySelector('#prompt-input').value = '';
      document.querySelector('#undo-button').click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, 'Number(document.querySelector(\'#session-history-list\').dataset.currentEditCount) === 0'),
      "operation identity test cleanup",
    );

    const incrementalBase = await evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      const originalFetch = window.fetch;
      window.__incrementalFailed = false;
      window.__incrementalPolls = 0;
      window.__incrementalProjectPending = false;
      window.__incrementalProjectReleased = false;
      window.__incrementalBaseEditCount = project.edits.length;
      const deferredProjectResponses = [];
      const published = structuredClone(project);
      published.version += 1;
      published.canUndo = true;
      published.edits.push({
        id: 900000,
        start: 8,
        end: 16,
        prompt: 'build this in stages',
        summary: 'Added the first staged layer',
        action: { type: 'gain', value: 1.1, target: 'all' },
      });
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/edits') {
          window.__incrementalOperationId = new URLSearchParams(options.body).get('operation_id');
          return new Response(JSON.stringify({
            id: 'incremental-job', operationId: window.__incrementalOperationId, status: 'queued', phase: 'queued',
            detail: 'Waiting for the edit worker', elapsedSeconds: 0, timeoutSeconds: 1200, pollAfterMs: 20,
            appliedSteps: 0, projectVersion: null,
          }), { status: 202, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/edits/incremental-job') {
          window.__incrementalPolls += 1;
          if (window.__incrementalFailed) {
            return new Response(JSON.stringify({
              id: 'incremental-job', operationId: window.__incrementalOperationId, status: 'failed',
              phase: 'failed', detail: 'Gemini stopped unexpectedly', elapsedSeconds: 2,
              timeoutSeconds: 1200, pollAfterMs: 20, appliedSteps: 1, projectVersion: published.version,
              error: 'Gemini stopped unexpectedly',
            }), {
              status: 200,
              headers: { 'Content-Type': 'application/json' },
            });
          }
          const job = {
            id: 'incremental-job', operationId: window.__incrementalOperationId, status: 'running',
            phase: 'editing', detail: 'Applied step 1 of 2: Added the first staged layer', elapsedSeconds: 1,
            timeoutSeconds: 1200, pollAfterMs: 20, appliedSteps: 1, projectVersion: published.version,
          };
          return new Response(JSON.stringify(job), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        if (resource === '/api/history' && window.__incrementalProjectReleased) {
          return new Response(JSON.stringify({
            current: 1,
            currentVersion: published.version,
            entries: [
              {
                index: 0, version: project.version, summary: 'Initial project', source: 'Project',
                prompt: null, start: null, end: null,
              },
              {
                index: 1, version: published.version, summary: 'Added the first staged layer', source: 'Gemini',
                prompt: 'build this in stages', start: 8, end: 16,
              },
            ],
          }), { status: 200, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/project') {
          if (window.__incrementalProjectReleased) {
            return new Response(JSON.stringify(published), {
              status: 200,
              headers: { 'Content-Type': 'application/json' },
            });
          }
          window.__incrementalProjectPending = true;
          return new Promise((resolve) => deferredProjectResponses.push(() => resolve(new Response(
            JSON.stringify(published),
            { status: 200, headers: { 'Content-Type': 'application/json' } },
          ))));
        }
        return originalFetch(resource, options);
      };
      window.__releaseIncrementalProject = () => {
        window.__incrementalProjectReleased = true;
        window.__incrementalProjectPending = false;
        for (const resolve of deferredProjectResponses.splice(0)) resolve();
      };
      window.__restoreIncrementalFetch = () => { window.fetch = originalFetch; };
      const input = document.querySelector('#prompt-input');
      input.value = 'build this in stages';
      document.querySelector('#prompt-form').requestSubmit();
      return { version: project.version, edits: project.edits.length };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `window.__incrementalProjectPending &&
          document.querySelector('#edit-progress-label').textContent === 'Showing Gemini step 1'`,
      ),
      "delayed incremental Gemini project refresh",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        width: document.querySelector('#edit-progress-fill').style.width,
        ariaText: document.querySelector('#edit-progress-track').getAttribute('aria-valuetext'),
      })`),
      { width: "55%", ariaText: "Showing Gemini step 1" },
      "project syncing must preserve the current edit activity progress",
    );
    await evaluate(cdp, appSession, "window.__releaseIncrementalProject()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `window.__incrementalPolls >= 1 &&
          Number(document.querySelector('#session-history-list').dataset.currentEditCount) === window.__incrementalBaseEditCount + 1 &&
          document.querySelector('[data-history-source="Gemini"] strong').textContent === 'Added the first staged layer' &&
          !document.querySelector('#compose-button').disabled &&
          document.querySelector('#edit-progress-label').textContent ===
            'Applied step 1 of 2: Added the first staged layer' &&
          document.querySelector('#edit-progress-fill').style.width === '55%' &&
          document.querySelector('#edit-progress-track').getAttribute('aria-valuetext') ===
            '1 edit step applied. Applied step 1 of 2: Added the first staged layer'`,
      ),
      "incremental Gemini project publication",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "fetch('/api/project').then((response) => response.json()).then((project) => project.edits.at(-1).operationId ?? null)",
      ),
      null,
      "an intermediate batch must not expose the terminal operation marker",
    );
    await evaluate(cdp, appSession, "window.__incrementalFailed = true");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `!document.querySelector('#compose-button').disabled &&
          document.querySelector('#toast-message').textContent ===
            'Gemini stopped unexpectedly. 1 partial change was saved; review the project before retrying.' &&
          document.querySelector('#toast').classList.contains('is-error') &&
          document.querySelector('#prompt-input').value === '' &&
          Number(document.querySelector('#session-history-list').dataset.currentEditCount) === window.__incrementalBaseEditCount + 1 &&
          localStorage.getItem('daw-ai.pending-edit.v1') === null`,
      ),
      "partial edit warning after terminal failure",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        window.__restoreIncrementalFetch();
        return {
          version: Number(document.querySelector('#saved-state').textContent.replace('Version ', '')),
          edits: Number(document.querySelector('#session-history-list').dataset.currentEditCount),
        };
      })()`),
      { version: incrementalBase.version + 1, edits: incrementalBase.edits + 1 },
      "a failed partial edit must remain visible without leaving its prompt ready to resubmit",
    );

    const acceptanceLossBase = await evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      const originalFetch = window.fetch;
      const published = structuredClone(project);
      published.version += 1;
      published.canUndo = true;
      published.edits.push({
        id: 900001,
        start: 8,
        end: 16,
        prompt: 'add a layer after uncertain acceptance',
        summary: 'Added a layer before acceptance was confirmed',
        action: { type: 'gain', value: 1.05, target: 'all' },
      });
      window.__acceptanceLossPosts = 0;
      window.__acceptanceLossPolls = 0;
      window.__acceptanceLossInterrupts = 0;
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/edits') {
          window.__acceptanceLossPosts += 1;
          window.__acceptanceLossOperationId = new URLSearchParams(options.body).get('operation_id');
          if (!published.editOperations.some(
            (operation) => operation.operationId === window.__acceptanceLossOperationId
          )) {
            published.editOperations.push({
              operationId: window.__acceptanceLossOperationId,
              status: 'partial',
              appliedSteps: 1,
              projectVersion: published.version,
              message: 'Added a layer before acceptance was confirmed',
            });
          }
          const status = window.__acceptanceLossPosts === 1 ? 504 : 404;
          return new Response(JSON.stringify({ error: 'Simulated lost edit acceptance response' }), {
            status,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        if (typeof resource === 'string' && resource.startsWith('/api/edit-operations/')) {
          return new Response(JSON.stringify({
            id: 'recovered', operationId: window.__acceptanceLossOperationId, status: 'running', phase: 'editing',
            detail: 'Recovered active edit', elapsedSeconds: 1, pollAfterMs: 20,
            timeoutSeconds: 1200, appliedSteps: 1, projectVersion: published.version,
          }), { status: 200, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/edits/recovered/interrupt') {
          window.__acceptanceLossInterrupts += 1;
          return new Response(JSON.stringify({ status: 'interrupted' }), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        if (resource === '/api/edits/recovered') {
          window.__acceptanceLossPolls += 1;
          const interrupted = window.__acceptanceLossInterrupts > 0;
          return new Response(JSON.stringify({
            id: 'recovered', operationId: window.__acceptanceLossOperationId,
            status: interrupted ? 'failed' : 'running', phase: interrupted ? 'failed' : 'editing',
            detail: interrupted ? 'Edit interrupted by the user.' : 'Recovered active edit',
            errorStatus: interrupted ? 409 : undefined,
            error: interrupted ? 'Edit interrupted by the user.' : undefined,
            elapsedSeconds: 1, pollAfterMs: 20, timeoutSeconds: 1200,
            appliedSteps: 1, projectVersion: published.version,
          }), { status: 200, headers: { 'Content-Type': 'application/json' } });
        }
        if (resource === '/api/project') {
          return new Response(JSON.stringify(published), {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        return originalFetch(resource, options);
      };
      window.__restoreFetchAfterAcceptanceLoss = () => { window.fetch = originalFetch; };
      const input = document.querySelector('#prompt-input');
      input.value = 'add a layer after uncertain acceptance';
      document.querySelector('#prompt-form').requestSubmit();
      return { version: project.version, edits: project.edits.length };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `window.__acceptanceLossPolls > 0 &&
          document.querySelector('#compose-button span').textContent === 'Interrupt'`,
      ),
      "interruptible recovered edit after acceptance loss",
    );
    await evaluate(cdp, appSession, "document.querySelector('#compose-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `!document.querySelector('#compose-button').disabled &&
          window.__acceptanceLossPosts === 2 &&
          window.__acceptanceLossInterrupts === 1 &&
          document.querySelector('#toast-message').textContent ===
            'Edit interrupted by the user. 1 partial change was saved; review the project before retrying.' &&
          document.querySelector('#toast').classList.contains('is-error') &&
          document.querySelector('#prompt-input').value === '' &&
          Number(document.querySelector('#session-history-list').dataset.currentEditCount) === ${acceptanceLossBase.edits + 1} &&
          localStorage.getItem('daw-ai.pending-edit.v1') === null`,
      ),
      "partial publication recovery after edit acceptance loss",
    );
    await evaluate(cdp, appSession, "window.__restoreFetchAfterAcceptanceLoss()");

    const clientAudioBoundary = await evaluate(cdp, appSession, `(async () => {
      const source = await fetch('/app.js').then((response) => response.text());
      const engine = source.slice(source.indexOf('class AudioEngine'), source.indexOf('const audio = new AudioEngine'));
      return {
        apiNoStore: source.includes('cache: "no-store"'),
        backendSpectrumTimeline: engine.includes('/api/track-spectrum/') && engine.includes('this.media.currentTime'),
        frontendWorkers: engine.includes('new Worker('),
        offlineContext: source.includes('OfflineAudioContext'),
        oscillator: source.includes('createOscillator'),
        backendEndpoint: source.includes('/api/audio-stream/'),
        trackSpectrumEndpoint: source.includes('/api/track-spectrum/'),
        timestampedSpectrum: engine.includes('frameDuration') && engine.includes('projectTime'),
        mediaClock: engine.includes('media.currentTime'),
        performanceClock: engine.includes('performance.now'),
        prefetch: engine.includes('beginPrefetch'),
        reusableMedia: (engine.match(/new Audio\(\)/g) || []).length,
        boundedRetry: engine.includes('AUDIO_RETRY_DELAYS_MS') && engine.includes('retryPlayback'),
      };
    })()`);
    assert.deepEqual(
      clientAudioBoundary,
      {
        apiNoStore: true,
        backendSpectrumTimeline: true,
        frontendWorkers: false,
        offlineContext: false,
        oscillator: false,
        backendEndpoint: true,
        trackSpectrumEndpoint: true,
        timestampedSpectrum: true,
        mediaClock: true,
        performanceClock: false,
        prefetch: false,
        reusableMedia: 1,
        boundedRetry: true,
      },
      "the browser client must use one retryable transport for backend-rendered audio",
    );

    const transportSyncVersion = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json()).then((project) => project.version)",
    );
    await evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      window.prompt = () => String(project.duration);
      document.querySelector('#ai-duration-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `fetch('/api/project').then((response) => response.json()).then(
          (project) => project.version > ${transportSyncVersion}
        )`,
      ),
      "transport fixture synchronization",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').disabled"),
      "spectrum readiness after transport fixture synchronization",
      60_000,
    );
    await evaluate(cdp, appSession, "document.querySelector('#rewind-button').click()");
    await evaluate(cdp, appSession, `(() => {
      window.__playingEventSync = null;
      window.__transportMedia.addEventListener('playing', () => {
        window.__playingEventSync = {
          mediaTime: window.__transportMedia.currentTime,
          displayedTime: document.querySelector('#current-time').textContent,
          state: document.documentElement.dataset.audioState,
        };
      }, { once: true });
    })()`);
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        "document.documentElement.dataset.audioState === 'playing'",
      ),
      "backend audio transport start",
      30_000,
    );
    const playingEventSync = await evaluate(cdp, appSession, "window.__playingEventSync");
    assert.equal(playingEventSync.state, "playing", "the transport must start on the media playing event");
    assert.match(playingEventSync.displayedTime, /^0:00\./);
    assert.ok(
      Math.abs(Number(playingEventSync.displayedTime.slice(2)) - playingEventSync.mediaTime) < 0.1,
      "the displayed playhead must use the media clock when audible playback starts",
    );
    const backendPlaybackTime = await evaluate(
      cdp,
      appSession,
      "document.querySelector('#current-time').textContent",
    );
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelector('#current-time').textContent !== ${JSON.stringify(backendPlaybackTime)}`,
      ),
      "backend audio transport movement",
    );
    await evaluate(cdp, appSession, `(async () => {
      const lane = document.querySelector('.track-lane');
      window.__rapidSeekPlayBaseline = window.__transportPlayCalls.length;
      for (let index = 0; index < 12; index += 1) {
        lane.dispatchEvent(new KeyboardEvent('keydown', {
          key: 'ArrowRight', bubbles: true, cancelable: true,
        }));
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.documentElement.dataset.audioState === 'playing' &&
          window.__transportPlayCalls.length === window.__rapidSeekPlayBaseline + 1`,
      ),
      "coalesced active keyboard seeks",
      30_000,
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "window.__transportPlayCalls.length - window.__rapidSeekPlayBaseline",
      ),
      1,
      "rapid active seeks must start only the final requested stream",
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        "!document.querySelector('#play-button').classList.contains('is-playing')",
      ),
      "backend audio transport pause",
    );

    await evaluate(cdp, appSession, `(() => {
      const lane = document.querySelector('.track-lane');
      lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }));
      for (let index = 0; index < 63; index += 1) {
        lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
      }
      window.__audioPlayCountBeforeBoundary = window.__transportPlayCalls.length;
      document.querySelector('#play-button').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.documentElement.dataset.audioState === 'playing' &&
          window.__transportPlayCalls.length === window.__audioPlayCountBeforeBoundary + 1 &&
          window.__transportMedia.getAttribute('src').startsWith('/api/audio-stream/')`,
      ),
      "continuous backend audio stream",
      30_000,
    );
    const playCountBeforeRetry = await evaluate(
      cdp,
      appSession,
      "window.__transportPlayCalls.length",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__audioAccessRefreshes = 0;
      window.fetch = (...args) => {
        const requestUrl = typeof args[0] === 'string' ? args[0] : args[0]?.url;
        if (requestUrl === '/api/audio-access') window.__audioAccessRefreshes += 1;
        return originalFetch(...args);
      };
      window.__restoreFetchAfterAudioRetry = () => {
        window.fetch = originalFetch;
      };
      window.__transportMedia.dispatchEvent(new Event('error'));
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.documentElement.dataset.audioState === 'playing' &&
          window.__transportPlayCalls.length === ${playCountBeforeRetry + 1}`,
      ),
      "successful retry after a transient audio stream failure",
      30_000,
    );
    const retriedTransport = await evaluate(cdp, appSession, `({
      sameElement: window.__transportPlayCalls.every((call) => call.sameElement),
      latestSource: window.__transportPlayCalls.at(-1).source,
      previousSource: window.__transportPlayCalls.at(-2).source,
    })`);
    assert.equal(retriedTransport.sameElement, true, "every playback and retry must reuse one media element");
    assert.equal(
      await evaluate(cdp, appSession, "window.__audioAccessRefreshes"),
      1,
      "a playback retry must refresh the stream token after a service restart",
    );
    await evaluate(cdp, appSession, "window.__restoreFetchAfterAudioRetry()");
    assert.notEqual(
      retriedTransport.latestSource,
      retriedTransport.previousSource,
      "a retry must request a fresh stream from the preserved playhead",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__audioBoundaryPlayCount = window.__transportPlayCalls.length;
      window.__audioBoundarySource = window.__transportMedia.getAttribute('src');
      window.__audioBoundaryStates = [document.documentElement.dataset.audioState];
      window.__audioBoundaryObserver = new MutationObserver(() => {
        window.__audioBoundaryStates.push(document.documentElement.dataset.audioState);
      });
      window.__audioBoundaryObserver.observe(document.documentElement, {
        attributes: true,
        attributeFilter: ['data-audio-state'],
      });
      window.__transportMedia.currentTime = 16.05;
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.documentElement.dataset.audioState === 'playing' &&
          document.querySelector('#current-time').textContent.startsWith('0:31.')`,
      ),
      "playback across the backend render boundary",
      30_000,
    );
    const audioBoundary = await evaluate(cdp, appSession, `(() => {
      window.__audioBoundaryObserver.disconnect();
      return {
        state: document.documentElement.dataset.audioState,
        time: document.querySelector('#current-time').textContent,
        restarted: window.__audioBoundaryStates.includes('starting'),
        playCalls: window.__transportPlayCalls.length - window.__audioBoundaryPlayCount,
        sameSource: window.__transportMedia.getAttribute('src') === window.__audioBoundarySource,
      };
    })()`);
    assert.equal(audioBoundary.state, "playing");
    assert.equal(audioBoundary.restarted, false, "render boundaries must not restart the transport");
    assert.equal(audioBoundary.playCalls, 0, "render boundaries must not invoke another media player");
    assert.equal(audioBoundary.sameSource, true, "render boundaries must remain in one media resource");
    assert.match(audioBoundary.time, /^0:31\./);
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.documentElement.dataset.audioState === 'idle'"),
      "boundary playback pause",
    );

    const backendRenderChange = await evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      const access = await fetch('/api/audio-access', {
        headers: { 'X-DAW-AI-Audio': '1' },
      }).then((response) => response.json());
      const render = async (version) => {
        const response = await fetch(
          '/api/audio-stream/' + encodeURIComponent(access.streamToken) + '/' + version + '/0',
          { headers: { Range: 'bytes=44-4095' } },
        );
        if (!response.ok) throw new Error(await response.text());
        return new Uint8Array(await response.arrayBuffer());
      };
      const bass = project.tracks.find((track) => track.role === 'bass');
      const before = await render(project.version);
      const changedVolume = bass.volume > 0.4 ? 0.25 : 0.75;
      const changed = await fetch('/api/mix', {
        method: 'POST',
        headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
        body: new URLSearchParams({ track_id: bass.id, volume: changedVolume }),
      });
      if (!changed.ok) throw new Error(await changed.text());
      const changedProject = await changed.json();
      const after = await render(changedProject.version);
      const undone = await fetch('/api/undo', { method: 'POST' });
      if (!undone.ok) throw new Error(await undone.text());
      return {
        beforeVersion: project.version,
        afterVersion: changedProject.version,
        changed: before.some((value, index) => value !== after[index]),
      };
    })()`);
    assert.equal(backendRenderChange.afterVersion, backendRenderChange.beforeVersion + 1);
    assert.equal(
      backendRenderChange.changed,
      true,
      "a sound-graph mutation must change the backend-rendered PCM",
    );

    const beforeNativeOverrides = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json()).then((project) => JSON.stringify(project.tracks.find((track) => track.id === 2).instrument.nativeOverrides))",
    );
    attacker = await startAttackerServer(attackerPort);
    const attackerSession = await openPage(cdp, `http://127.0.0.1:${attackerPort}`);
    await evaluate(cdp, attackerSession, `fetch('${appUrl}/api/edits', {
      method: 'POST',
      mode: 'no-cors',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: 'start=1&end=2&prompt=hostile+edit'
    }).then(() => true).catch(() => false)`);
    await evaluate(cdp, attackerSession, `fetch('${appUrl}/api/sound-tools', {
      method: 'POST',
      mode: 'no-cors',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body: 'track_id=2&tool=instrument&tool_id=201&parameter=cutoff&value=0.9'
    }).then(() => true).catch(() => false)`);
    const afterAttack = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
    assert.equal(afterAttack.edits.some((edit) => edit.prompt === "hostile edit"), false);
    assert.equal(
      JSON.stringify(afterAttack.tracks.find((track) => track.id === 2).instrument.nativeOverrides),
      beforeNativeOverrides,
      "cross-origin sound-tool mutations must be rejected",
    );
    assert.equal(consoleErrors.length, 0, "application emitted browser console errors");

    console.log(
      "Browser workflows passed: mobile layout/panning, keyboard selection, backend audio rendering/transport, studio tabs/debug report, prompt single-flight/undo, cross-origin guard",
    );
  } finally {
    if (attacker) await new Promise((resolve) => attacker.close(resolve));
    await closeBrowser(cdp, chrome);
    await terminate(app);
    await removeBrowserProfile(profile);
  }

  if (appErrors) process.stderr.write(appErrors);
  if (chrome.exitCode && chromeErrors) process.stderr.write(chromeErrors);
}

run().catch((error) => {
  console.error(error.stack || error);
  process.exitCode = 1;
});
