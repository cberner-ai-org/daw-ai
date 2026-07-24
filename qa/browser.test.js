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
  const app = spawn(path.join(root, "target", "debug", "daw-ai"), ["--port", String(appPort)], {
    cwd: root,
    env: {
      ...process.env,
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
    );
    await cdp.send("Target.closeTarget", { targetId: startupPage.targetId });
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
      channels: 1,
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
      await evaluate(
        cdp,
        appSession,
        "document.querySelector('#advanced-drawer').hidden && document.querySelector('#advanced-drawer').inert",
      ),
      true,
      "closed advanced controls must be inert",
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
      };
    })()`);
    assert.deepEqual(
      debugView.tabs,
      [
        { name: "AI Mode", selected: "false" },
        { name: "Advanced", selected: "false" },
        { name: "Debug", selected: "true" },
      ],
      "the three chartered studio views must be exposed as tabs",
    );
    assert.equal(debugView.debugVisible && debugView.aiHidden, true, "Debug must replace the AI Mode panel");
    assert.match(debugView.report, /Synthetic browser failure/);
    assert.match(debugView.report, /Backend warnings and errors are written/);
    assert.match(debugView.report, /Gemini sessions: 0 retained locally/);
    assert.match(
      await evaluate(cdp, appSession, "document.querySelector('#gemini-session-list').textContent"),
      /No Gemini sessions recorded yet/,
    );
    const sessions = await evaluate(
      cdp,
      appSession,
      "fetch('/api/gemini-sessions').then((response) => response.json())",
    );
    assert.deepEqual(sessions, { sessions: [] }, "Gemini sessions must be persistently listable");
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
      "advanced-button",
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
    const restoredOverlays = await evaluate(cdp, appSession, `(() => {
      const selection = document.querySelector('#timeline-selection');
      const playhead = document.querySelector('#playhead');
      const layout = () => ({
        selectionLeft: selection.style.left,
        selectionWidth: selection.style.width,
        playheadLeft: playhead.style.left,
      });
      const before = layout();
      document.querySelector('#advanced-button').click();
      window.dispatchEvent(new Event('resize'));
      const hidden = layout();
      document.querySelector('#ai-mode-button').click();
      const after = layout();
      return { before, hidden, after };
    })()`);
    assert.notDeepEqual(
      restoredOverlays.hidden,
      restoredOverlays.before,
      "hidden layout must exercise the zero-width regression",
    );
    assert.deepEqual(
      restoredOverlays.after,
      restoredOverlays.before,
      `returning to AI Mode must restore timeline overlays (${JSON.stringify(restoredOverlays)})`,
    );

    await cdp.send(
      "Emulation.setDeviceMetricsOverride",
      { width: 390, height: 844, deviceScaleFactor: 1, mobile: true },
      appSession,
    );
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 1 }, appSession);
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "mobile advanced drawer",
    );
    const mobileAdvancedBounds = await evaluate(cdp, appSession, `(() => {
      const drawer = document.querySelector('#advanced-drawer');
      const drawerRect = drawer.getBoundingClientRect();
      const controls = [...drawer.querySelectorAll('.range-with-output, .piano-roll')];
      return {
        bodyWidth: document.body.scrollWidth,
        viewportWidth: document.documentElement.clientWidth,
        drawerClientWidth: drawer.clientWidth,
        drawerScrollWidth: drawer.scrollWidth,
        drawerRight: drawerRect.right,
        widestControlRight: Math.max(...controls.map((control) => control.getBoundingClientRect().right)),
      };
    })()`);
    assert.ok(
      mobileAdvancedBounds.bodyWidth <= mobileAdvancedBounds.viewportWidth &&
        mobileAdvancedBounds.drawerScrollWidth <= mobileAdvancedBounds.drawerClientWidth &&
        mobileAdvancedBounds.widestControlRight <= mobileAdvancedBounds.drawerRight,
      `Advanced controls must fit a 390px viewport (${JSON.stringify(mobileAdvancedBounds)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "mobile advanced drawer close",
    );
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
    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "desktop advanced drawer",
    );
    const desktopAdvancedBounds = await evaluate(cdp, appSession, `(() => {
      const drawer = document.querySelector('#advanced-drawer');
      const channels = drawer.querySelector('.channel-list');
      return {
        drawerClientWidth: drawer.clientWidth,
        drawerScrollWidth: drawer.scrollWidth,
        channelsClientWidth: channels.clientWidth,
        channelsScrollWidth: channels.scrollWidth,
      };
    })()`);
    assert.ok(
      desktopAdvancedBounds.drawerScrollWidth <= desktopAdvancedBounds.drawerClientWidth &&
        desktopAdvancedBounds.channelsScrollWidth <= desktopAdvancedBounds.channelsClientWidth,
      `Advanced controls must not overflow at 1440x900 (${JSON.stringify(desktopAdvancedBounds)})`,
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "desktop advanced drawer close",
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
      window.fetch = function fetch(resource, options) {
        if (resource === '/api/edits') {
          window.__refusedEditRequests += 1;
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
      const input = document.querySelector('#prompt-input');
      input.value = 'refused edit';
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
    assert.equal(
      await evaluate(cdp, appSession, "window.__refusedEditRequests"),
      1,
      "an explicit acceptance refusal must not retry for the edit execution window",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterRefusedEdit();
      document.querySelector('#prompt-input').value = '';
    })()`);

    await evaluate(cdp, appSession, `(() => {
      const originalPlay = HTMLMediaElement.prototype.play;
      window.__transportMedia = null;
      window.__transportPlayCalls = [];
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
    const compoundPills = await evaluate(
      cdp,
      appSession,
      "[...document.querySelectorAll('.effect-pill.is-regional')].map((pill) => pill.textContent)",
    );
    assert.equal(compoundPills.some((pill) => /Reverb.*42%/.test(pill)), true);
    assert.equal(
      compoundPills.some((pill) => /Tone filter.*-30%/.test(pill)),
      true,
      "Advanced must expose the filter half of a compound edit",
    );

    const projectBeforeConflict = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__conflictProjectRefreshes = 0;
      window.__conflictPollCount = 0;
      window.__mixDuringPromptRequests = 0;
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
        if (resource === '/api/mix') window.__mixDuringPromptRequests += 1;
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
      const volume = document.querySelector('[data-volume-track="1"]');
      volume.value = '0.51';
      volume.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `window.__mixDuringPromptRequests === 1 &&
          document.querySelector('[data-volume-track="1"]').value === '0.51' &&
          !document.querySelector('#compose-button').disabled`,
      ),
      "manual mixer mutation during accepted edit polling",
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
    assert.equal(conflictProject.tracks[0].volume, 0.51);
    assert.equal(
      await evaluate(cdp, appSession, "window.__mixDuringPromptRequests"),
      1,
      "accepted edit polling must not own the project mutation queue",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('[data-volume-track=\"1\"]').value"),
      "0.51",
      "a 409 edit must render the newer authoritative project before unlocking",
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
        `document.querySelector('[data-volume-track="1"]').value === ${JSON.stringify(String(projectBeforeConflict.tracks[0].volume))}`,
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
              body: new URLSearchParams({ prompt: 'increase volume', start: '4', end: '8' }),
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

    await evaluate(cdp, appSession, "document.querySelector('#advanced-button').click()");
    await waitFor(
      async () =>
        evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').classList.contains('is-open')"),
      "advanced drawer",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        `!document.querySelector('#advanced-drawer').hidden &&
          !document.querySelector('#advanced-drawer').inert &&
          document.querySelector('#ai-mode-panel').hidden &&
          document.querySelector('#advanced-button').getAttribute('aria-selected') === 'true'`,
      ),
      true,
      "Advanced must replace AI Mode as the active full-page tab",
    );
    const channelsBefore = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json()).then((project) => project.tracks.length)",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__unrelatedChannelInjected = false;
      window.fetch = async function fetch(resource, options) {
        if (resource === '/api/channels' && !window.__unrelatedChannelInjected) {
          const request = new URLSearchParams(options.body);
          window.__lostChannelOperationId = request.get('operation_id');
          window.__unrelatedChannelInjected = true;
          await originalFetch('/api/channels', {
            method: 'POST',
            headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
            body: new URLSearchParams({ action: 'add', role: 'bass' }),
          });
          return new Response('not valid JSON', {
            status: 200,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        return originalFetch(resource, options);
      };
      window.__restoreFetchAfterUnrelatedChannel = () => { window.fetch = originalFetch; };
      const role = document.querySelector('#channel-role');
      role.value = 'lead';
      document.querySelector('#channel-creator').requestSubmit();
    })()`);
    const unrelatedChannel = await waitFor(
      async () => evaluate(cdp, appSession, `(async () => {
        if (document.querySelectorAll('.channel-card').length !== ${channelsBefore + 1} ||
            document.querySelector('#add-channel').disabled ||
            !document.querySelector('#toast').classList.contains('is-error')) return false;
        const project = await fetch('/api/project').then((response) => response.json());
        const unrelated = project.tracks.at(-1);
        const card = document.querySelector('[data-channel-track="' + unrelated.id + '"]');
        if (unrelated.role !== 'bass' || document.activeElement === card ||
            project.channelOperations.some(
              (operation) => operation.operationId === window.__lostChannelOperationId
            )) return false;
        return { id: unrelated.id, toast: document.querySelector('#toast-message').textContent };
      })()`),
      "unrelated concurrent channel must not confirm a lost add",
    );
    assert.notEqual(unrelatedChannel.toast, "Track added");
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterUnrelatedChannel();
      window.confirm = () => true;
      document.querySelector('[data-delete-track="${unrelatedChannel.id}"]').click();
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, `document.querySelectorAll('.channel-card').length === ${channelsBefore}`),
      "unrelated channel test cleanup",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.__ambiguousChannelResponses = 0;
      window.fetch = async function fetch(resource, options) {
        const response = await originalFetch(resource, options);
        if (resource === '/api/channels' && response.ok) {
          window.__ambiguousChannelResponses += 1;
          return new Response('not valid JSON', {
            status: response.status,
            headers: { 'Content-Type': 'application/json' },
          });
        }
        return response;
      };
      window.__restoreFetchAfterChannels = () => { window.fetch = originalFetch; };
    })()`);
    await evaluate(cdp, appSession, `(() => {
      const role = document.querySelector('#channel-role');
      role.value = 'lead';
      document.querySelector('#channel-creator').requestSubmit();
    })()`);
    const addedChannel = await waitFor(
      async () => evaluate(cdp, appSession, `(async () => {
        const project = await fetch('/api/project').then((response) => response.json());
        if (project.tracks.length !== ${channelsBefore + 1}) return false;
        const lead = project.tracks.at(-1);
        const card = document.querySelector('[data-channel-track="' + lead.id + '"]');
        if (!card || document.activeElement !== card) return false;
        return {
          id: lead.id,
          role: lead.role,
          volume: lead.volume,
          preset: lead.instrument.preset,
          clips: lead.clips.length,
          effects: lead.effects.length,
          modulators: lead.modulators.length,
        };
      })()`),
      "Advanced channel creation",
    );
    assert.deepEqual(
      addedChannel,
      {
        id: addedChannel.id,
        role: "lead",
        volume: 1,
        preset: "Init",
        clips: 0,
        effects: 0,
        modulators: 0,
      },
      "a new Advanced channel must be neutral until explicitly configured",
    );
    await evaluate(cdp, appSession, `(() => {
      window.confirm = () => true;
      document.querySelector('[data-delete-track="${addedChannel.id}"]').click();
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelectorAll('.channel-card').length === ${channelsBefore} &&
          document.activeElement === document.querySelector('#add-channel')`,
      ),
      "Advanced channel deletion",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__ambiguousChannelResponses"),
      2,
      "ambiguous add and delete responses must reconcile without duplicate submissions",
    );
    await evaluate(cdp, appSession, "window.__restoreFetchAfterChannels()");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, `document.querySelectorAll('.channel-card').length === ${channelsBefore + 1}`),
      "Advanced channel deletion undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, `document.querySelectorAll('.channel-card').length === ${channelsBefore}`),
      "Advanced channel creation undo",
    );
    const summarySpace = await evaluate(cdp, appSession, `(() => {
      const finalClip = [...document.querySelectorAll('.clip-editor')].at(-1);
      finalClip.open = true;
      const summary = finalClip.querySelector('summary');
      summary.focus();
      const allowed = summary.dispatchEvent(
        new KeyboardEvent('keydown', { key: ' ', code: 'Space', bubbles: true, cancelable: true }),
      );
      return {
        allowed,
        focused: document.activeElement === summary,
        playing: document.querySelector('#play-button').classList.contains('is-playing'),
      };
    })()`);
    assert.deepEqual(
      summarySpace,
      { allowed: true, focused: true, playing: false },
      `Space on a clip summary must remain native and transport-neutral (${JSON.stringify(summarySpace)})`,
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const select = document.querySelector('[data-sound-tool="instrument"][data-parameter="preset"]');
        select.focus();
        const allowed = select.dispatchEvent(
          new KeyboardEvent('keydown', { key: ' ', code: 'Space', bubbles: true, cancelable: true }),
        );
        return {
          allowed,
          playing: document.querySelector('#play-button').classList.contains('is-playing'),
        };
      })()`),
      { allowed: true, playing: false },
      "Space on a select must remain available to the native control",
    );
    assert.equal(
      await evaluate(cdp, appSession, `(() => {
        const finalClip = [...document.querySelectorAll('.clip-editor')].at(-1);
        finalClip.open = false;
        return finalClip.open;
      })()`),
      false,
      "clip event disclosures must remain collapsible in the Advanced tab",
    );
    await evaluate(cdp, appSession, "[...document.querySelectorAll('.clip-editor')].at(-1).open = true");
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        instruments: document.querySelectorAll('.instrument-tool').length,
        effects: document.querySelectorAll('.effects-tool').length,
        modulators: document.querySelectorAll('.modulator-card').length,
        routes: document.querySelectorAll('.routing-chain').length,
        noteTables: document.querySelectorAll('.clip-event-list, .clip-event').length,
      })`),
      { instruments: 3, effects: 3, modulators: 3, routes: 3, noteTables: 0 },
      "Advanced must expose every sound tool without duplicating MIDI notes in tables",
    );
    const advancedGraphSummary = await evaluate(cdp, appSession, `(() => ({
        graphs: document.querySelectorAll('.sound-graph').length,
        selectableNodes: document.querySelectorAll('[data-graph-node]').length,
        selectedNodes: document.querySelectorAll('[data-graph-node][aria-pressed="true"]').length,
        visibleInspectors: [...document.querySelectorAll('.node-inspector')].filter((pane) => !pane.hidden).length,
        pianoRolls: document.querySelectorAll('.piano-roll').length,
        visualNotes: document.querySelectorAll('.midi-note').length,
        editableNotes: document.querySelectorAll('button.midi-note[data-midi-event]').length,
      }))()`);
    assert.deepEqual(
      advancedGraphSummary,
      {
        graphs: 3,
        selectableNodes: 10,
        selectedNodes: 3,
        visibleInspectors: 3,
        pianoRolls: 3,
        visualNotes: 14,
        editableNotes: 14,
      },
      `Advanced must render selectable sound graphs and standard MIDI piano rolls (${JSON.stringify(advancedGraphSummary)})`,
    );
    const projectBeforePianoRollEdit = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('[data-clip-key="2-12"] [data-midi-event="1201"]').click();
      const pitch = document.querySelector(
        '[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]',
      );
      pitch.value = '34';
      pitch.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforePianoRollEdit.version + 1 &&
        project.tracks[1].clips[0].events[0].pitch === 34;
    }, "piano-roll note edit");
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "document.activeElement.dataset.controlKey",
      ),
      "2-clip-12-event-1201-pitch",
      "piano-roll note edits must restore focus to the selected note inspector",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].clips[0].events[0].pitch === 33;
    }, "piano-roll note edit undo");
    await evaluate(cdp, appSession, `document.querySelector(
      '[data-graph-node="effect:210"][data-track-id="2"]',
    ).click()`);
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const card = document.querySelector('[data-channel-track="2"]');
        return {
          selected: card.querySelector('[data-graph-node="effect:210"]').getAttribute('aria-pressed'),
          effectPaneVisible: !card.querySelector('.effects-tool').hidden,
          instrumentPaneHidden: card.querySelector('.instrument-tool').hidden,
          focusedNode: document.activeElement.dataset.graphNode,
        };
      })()`),
      {
        selected: "true",
        effectPaneVisible: true,
        instrumentPaneHidden: true,
        focusedNode: "effect:210",
      },
      "clicking a graph node must reveal and focus its parameter side pane",
    );
    await evaluate(cdp, appSession, `document.querySelector(
      '[data-graph-node="instrument:201"][data-track-id="2"]',
    ).click()`);
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        engines: [...document.querySelectorAll('.instrument-tool .tool-heading strong')]
          .map((heading) => heading.textContent),
        presets: [...document.querySelectorAll('[data-sound-tool="instrument"][data-parameter="preset"]')]
          .map((control) => control.value),
        nativeGroups: {
          common: Boolean(document.querySelector('[data-channel-track="2"] [data-load-instrument-parameters="2:common"]')),
          advanced: Boolean(document.querySelector('[data-channel-track="2"] [data-load-instrument-parameters="2:advanced"]')),
        },
        filterParameters: [...document.querySelectorAll(
          'input[data-sound-tool="effect"][data-track-id="2"][data-tool-id="210"]',
        )].map((control) => control.dataset.parameter),
        midiRoutes: document.querySelectorAll('.modulator-route').length,
        rateModes: [...document.querySelectorAll('[data-sound-tool="modulator"][data-parameter="rateMode"]')]
          .map((control) => control.value),
        triggers: [...document.querySelectorAll('[data-sound-tool="modulator"][data-parameter="trigger"]')]
          .map((control) => control.value),
      })`),
      {
        engines: ["Surge XT", "Surge XT", "Surge XT"],
        presets: ["Surge Kick", "Surge Bass", "Surge Pad"],
        nativeGroups: { common: true, advanced: true },
        filterParameters: [
          "mix",
          "cutoff",
          "resonance",
          "highFrequency",
          "highGain",
          "lowFrequency",
          "lowGain",
          "midFrequency",
          "midGain",
        ],
        midiRoutes: 1,
        rateModes: ["hz", "hz", "hz"],
        triggers: ["midi", "free", "free"],
      },
      "Advanced must expose Surge presets, native parameters, and modulator sync/trigger controls",
    );
    await evaluate(cdp, appSession, `(() => {
      const preset = document.querySelector(
        '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="preset"]',
      );
      preset.value = 'Surge Pad';
      preset.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      const instrument = project.tracks[1].instrument;
      return instrument.engine === 'Surge XT' && instrument.preset === 'Surge Pad' &&
        document.querySelector(
          '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="preset"]',
        ).value === 'Surge Pad';
    })()`), "Surge XT preset update");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      return project.tracks[1].instrument.preset === 'Surge Bass';
    })()`), "Surge XT preset undo");
    await evaluate(cdp, appSession, `document.querySelector(
      '[data-sound-tool="modulator"][data-track-id="1"][data-parameter="enabled"]',
    ).click()`);
    await waitFor(async () => evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      return !project.tracks[0].modulators[0].enabled &&
        document.querySelectorAll('.modulator-route').length === 0;
    })()`), "disabled MIDI modulator route removal");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => evaluate(cdp, appSession, `(async () => {
      const project = await fetch('/api/project').then((response) => response.json());
      return project.tracks[0].modulators[0].enabled &&
        document.querySelectorAll('.modulator-route').length === 1;
    })()`), "enabled MIDI modulator route restore");
    assert.deepEqual(
      await evaluate(cdp, appSession, `fetch('/api/project')
        .then((response) => response.json())
        .then((project) => {
        const chain = document.querySelector('.routing-chain');
        return {
          labels: [...chain.querySelectorAll('i b')].map((label) => label.textContent),
          types: project.tracks[0].routing.edges.map((edge) => edge.type),
        };
      })`),
      { labels: ["MIDI", "AUDIO", "AUDIO"], types: ["midi", "midi", "audio", "audio", "control"] },
      "Advanced and project routing must expose compatible edge types",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `[
        ...document.querySelectorAll(
          '[data-parameter="enabled"]:is([data-sound-tool="effect"], [data-sound-tool="modulator"])',
        ),
      ].map((button) => ({ name: button.getAttribute('aria-label'), pressed: button.getAttribute('aria-pressed') }))`),
      [
        { name: "Disable Pulse Kit Punch compressor effect #110", pressed: "true" },
        { name: "Disable Pulse Kit Pulse envelope modulator #150", pressed: "true" },
        { name: "Disable Soft Current Low-pass filter effect #210", pressed: "true" },
        { name: "Disable Soft Current Bass movement modulator #250", pressed: "true" },
        { name: "Disable Glass Chords Chorus effect #310", pressed: "true" },
        { name: "Disable Glass Chords Room effect #311", pressed: "true" },
        { name: "Disable Glass Chords Slow bloom modulator #350", pressed: "true" },
      ],
      "sound-tool toggles must expose contextual names and pressed state",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const controls = [...document.querySelectorAll(
          '[data-sound-tool]:is(input, select, button)',
        )];
        const names = controls.map((control) => control.getAttribute('aria-label'));
        return {
          allNamed: names.every(Boolean),
          allUnique: new Set(names).size === names.length,
          chordMixes: [...document.querySelectorAll(
            '[data-sound-tool="effect"][data-track-id="3"][data-parameter="mix"]',
          )].map((control) => control.getAttribute('aria-label')),
          chordRouting: [...document.querySelectorAll(
            '[data-sound-tool="routing"][data-track-id="3"]',
          )].map((control) => control.getAttribute('aria-label')),
        };
      })()`),
      {
        allNamed: true,
        allUnique: true,
        chordMixes: [
          "Glass Chords Chorus effect #310 mix",
          "Glass Chords Room effect #311 mix",
        ],
        chordRouting: [
          "Move Glass Chords Chorus effect #310 earlier",
          "Move Glass Chords Chorus effect #310 later",
          "Move Glass Chords Room effect #311 earlier",
          "Move Glass Chords Room effect #311 later",
        ],
      },
      "every repeated sound-tool control must have a unique contextual name",
    );
    await evaluate(
      cdp,
      appSession,
      "document.querySelector('[data-sound-tool=\"effect\"][data-tool-id=\"210\"][data-parameter=\"enabled\"]').click()",
    );
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return !project.tracks[1].effects[0].enabled;
    }, "disabled low-pass effect");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const button = document.querySelector(
          '[data-sound-tool="effect"][data-tool-id="210"][data-parameter="enabled"]',
        );
        return {
          name: button.getAttribute('aria-label'),
          pressed: button.getAttribute('aria-pressed'),
          text: button.textContent,
        };
      })()`),
      { name: "Enable Soft Current Low-pass filter effect #210", pressed: "false", text: "Off" },
      "a disabled sound tool must expose its updated action and state",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].effects[0].enabled;
    }, "disabled low-pass effect undo");
    await evaluate(cdp, appSession,
      "document.querySelector('[data-load-instrument-parameters=\"2:common\"]').click()");
    await waitFor(async () => evaluate(cdp, appSession,
      "Boolean(document.querySelector('[data-sound-tool=\"instrument\"][data-track-id=\"2\"][data-parameter^=\"native:\"]'))"),
    "native instrument controls");
    const nativeParameter = await evaluate(cdp, appSession,
      "document.querySelector('[data-sound-tool=\"instrument\"][data-track-id=\"2\"][data-parameter^=\"native:\"]').dataset.parameter");
    assert.equal(
      await evaluate(cdp, appSession, `Boolean(document.querySelector(
        '[data-sound-tool="modulator"][data-track-id="2"][data-parameter="formula"]',
      ))`),
      true,
      "Formula source must be reachable before changing a modulator shape",
    );
    assert.equal(
      await evaluate(cdp, appSession, `Boolean(document.querySelector(
        '[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"] option[value="${nativeParameter}"]',
      ))`),
      true,
      "loaded native parameters must appear as exact modulator targets",
    );
    await evaluate(cdp, appSession, `(() => {
      const target = document.querySelector(
        '[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"]',
      );
      target.value = '${nativeParameter}';
      target.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].modulators[0].target === nativeParameter;
    }, "persisted discovered native modulation target");
    assert.equal(
      await evaluate(cdp, appSession, `document.querySelector(
        '[data-sound-tool="modulator"][data-track-id="2"][data-parameter="target"]',
      ).value`),
      nativeParameter,
      "the selected native modulation target must survive rendering",
    );
    const projectBeforePreciseTools = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    const nativeBeforeRejectedUpdate = await evaluate(cdp, appSession, `(() => {
      const control = document.querySelector(
        '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="${nativeParameter}"]',
      );
      return { value: control.value, label: control.getAttribute('aria-label') };
    })()`);
    await evaluate(cdp, appSession, `(() => {
      const control = document.querySelector(
        '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="${nativeParameter}"]',
      );
      control.dataset.toolId = '999999';
      control.value = control.value === '0.123' ? '0.234' : '0.123';
      control.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => evaluate(cdp, appSession, `(
      document.querySelector('#toast').classList.contains('is-error') &&
      document.querySelector('#toast-message').textContent === 'sound tool not found'
    )`), "rejected native parameter update");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const control = document.querySelector(
          '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="${nativeParameter}"]',
        );
        return { value: control.value, label: control.getAttribute('aria-label') };
      })()`),
      nativeBeforeRejectedUpdate,
      "a rejected native parameter update must not change its cached value or override state",
    );
    assert.equal(
      (await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())")).version,
      projectBeforePreciseTools.version,
      "a rejected native parameter update must not change the project",
    );
    await evaluate(cdp, appSession, `(() => {
      const update = (selector, value) => {
        const control = document.querySelector(selector);
        control.value = value;
        control.dispatchEvent(new Event('change', { bubbles: true }));
      };
      update('[data-sound-tool="instrument"][data-track-id="2"][data-parameter="${nativeParameter}"]', '0.025');
      update('[data-sound-tool="effect"][data-track-id="3"][data-tool-id="310"][data-parameter="mix"]', '0.005');
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforePreciseTools.version + 2 &&
        project.tracks[1].instrument.nativeOverrides[nativeParameter.slice(7)] === 0.025 &&
        project.tracks[2].effects[0].parameters.mix === 0.005;
    }, "precise sound-tool values");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const release = document.querySelector(
          '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="${nativeParameter}"]',
        );
        const mix = document.querySelector(
          '[data-sound-tool="effect"][data-track-id="3"][data-tool-id="310"][data-parameter="mix"]',
        );
        return {
          release: { value: release.value, step: release.step, output: release.nextElementSibling.value },
          mix: {
            value: mix.value,
            step: mix.step,
            output: mix.nextElementSibling.value,
            heading: mix.closest('.effect-card').querySelector('.effect-pill b').textContent,
          },
        };
      })()`),
      {
        release: { value: "0.025", step: "any", output: "2.5%" },
        mix: { value: "0.005", step: "any", output: "0.5%", heading: "0.5%" },
      },
      "authoritative floats must render without range sanitization or display rounding",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[2].effects[0].parameters.mix === 0.28;
    }, "precise effect mix undo");
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[1].instrument.nativeOverrides?.[nativeParameter.slice(7)] === undefined;
    }, "precise native instrument control undo");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const control = document.querySelector(
          '[data-sound-tool="instrument"][data-track-id="2"][data-parameter="${nativeParameter}"]',
        );
        return { value: control.value, label: control.getAttribute('aria-label') };
      })()`),
      nativeBeforeRejectedUpdate,
      "undo must refresh a loaded native control from the authoritative project",
    );
    await evaluate(
      cdp,
      appSession,
      "document.querySelector('[data-sound-tool=\"routing\"][data-track-id=\"3\"][data-tool-id=\"311\"][data-sound-value=\"0\"]').click()",
    );
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const clientReady = await evaluate(
        cdp,
        appSession,
        "document.activeElement.dataset.controlKey === '3-routing-311-down'",
      );
      return project.tracks[2].routing.audio[2] === "effect:311" && clientReady;
    }, "advanced effect reorder");
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.dataset.controlKey"),
      "3-routing-311-down",
      "a reordered endpoint effect must focus its remaining enabled direction",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[2].routing.audio[2] === "effect:310";
    }, "advanced effect reorder undo");
    if (await evaluate(cdp, appSession, "Boolean(document.querySelector('.clip-event-list'))")) {
    const projectBeforeClipUiMutation = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const drumClip = document.querySelector('[data-clip-key="1-11"]');
      const bassClip = document.querySelector('[data-clip-key="2-12"]');
      const eventList = drumClip.querySelector('.clip-event-list');
      bassClip.open = false;
      eventList.scrollTop = eventList.scrollHeight;
      window.__clipScrollBeforeMutation = eventList.scrollTop;
      const input = document.querySelector(
        '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1112"][data-parameter="velocity"]',
      );
      input.value = '0.41';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforeClipUiMutation.version + 1 &&
        project.tracks[0].clips[0].events.at(-1).velocity === 0.41;
    }, "scrolled clip event mutation");
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const drumClip = document.querySelector('[data-clip-key="1-11"]');
        const eventList = drumClip.querySelector('.clip-event-list');
        const focused = document.activeElement;
        const listRect = eventList.getBoundingClientRect();
        const focusedRect = focused.getBoundingClientRect();
        return {
          collapsed: !document.querySelector('[data-clip-key="2-12"]').open,
          scrollTop: eventList.scrollTop,
          expectedScrollTop: window.__clipScrollBeforeMutation,
          focusKey: focused.dataset.controlKey,
          focusVisible: focusedRect.top >= listRect.top && focusedRect.bottom <= listRect.bottom,
        };
      })()`),
      {
        collapsed: true,
        scrollTop: await evaluate(cdp, appSession, "window.__clipScrollBeforeMutation"),
        expectedScrollTop: await evaluate(cdp, appSession, "window.__clipScrollBeforeMutation"),
        focusKey: "1-clip-11-event-1112-velocity",
        focusVisible: true,
      },
      "clip disclosure, nested scroll, and focused event state must survive authoritative rerenders",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[0].clips[0].events.at(-1).velocity === 0.42;
    }, "scrolled clip event undo");
    await evaluate(cdp, appSession, `(() => {
      document.querySelector('[data-clip-key="2-12"]').open = true;
      document.querySelector('[data-clip-key="1-11"] .clip-event-list').scrollTop = 0;
    })()`);
    const projectBeforeEventReorder = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector(
        '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="time"]',
      );
      input.value = '3.999';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.version === projectBeforeEventReorder.version + 1 &&
        project.tracks[0].clips[0].events.at(-1).id === 1101 &&
        project.tracks[0].clips[0].events.at(-1).time === 3.999;
    }, "event onset reorder");
    const reorderedEventFocus = await evaluate(cdp, appSession, `(() => {
      const eventList = document.querySelector('[data-clip-key="1-11"] .clip-event-list');
      const focused = document.activeElement;
      const listRect = eventList.getBoundingClientRect();
      const focusedRect = focused.getBoundingClientRect();
      return {
        key: focused.dataset.controlKey,
        scrollTop: eventList.scrollTop,
        visible: focusedRect.top >= listRect.top && focusedRect.bottom <= listRect.bottom,
        value: focused.value,
        maximum: focused.max,
        valid: focused.checkValidity(),
      };
    })()`);
    assert.ok(
      reorderedEventFocus.key === "1-clip-11-event-1101-time" &&
        reorderedEventFocus.scrollTop > 0 && reorderedEventFocus.visible &&
        reorderedEventFocus.value === "3.999" && reorderedEventFocus.maximum === "4" &&
        reorderedEventFocus.valid,
      `a reordered event must reveal its restored focus (${JSON.stringify(reorderedEventFocus)})`,
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const originalFetch = window.fetch;
        let requests = 0;
        window.fetch = function fetch(resource, options) {
          if (resource === '/api/sound-tools') requests += 1;
          return originalFetch(resource, options);
        };
        const input = document.querySelector(
          '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="time"]',
        );
        input.value = '4';
        input.dispatchEvent(new Event('change', { bubbles: true }));
        window.fetch = originalFetch;
        const restored = document.querySelector(
          '[data-sound-tool="event"][data-track-id="1"][data-tool-id="1101"][data-parameter="time"]',
        );
        return { requests, value: restored.value, maximum: restored.max, valid: restored.checkValidity() };
      })()`),
      { requests: 0, value: "3.999", maximum: "4", valid: true },
      "the event-time endpoint must be represented but rejected as an exclusive bound",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      return project.tracks[0].clips[0].events[0].id === 1101 && project.tracks[0].clips[0].events[0].time === 0;
    }, "event onset reorder undo");
    await evaluate(
      cdp,
      appSession,
      "document.querySelector('[data-clip-key=\"1-11\"] .clip-event-list').scrollTop = 0",
    );
    const projectBeforeRejectedSoundTool = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const originalFetch = window.fetch;
        let requests = 0;
        window.fetch = function fetch(resource, options) {
          if (resource === '/api/sound-tools') requests += 1;
          return originalFetch(resource, options);
        };
        const input = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
        input.value = '40.5';
        input.dispatchEvent(new Event('change', { bubbles: true }));
        window.fetch = originalFetch;
        const restored = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
        return { requests, value: restored.value, valid: restored.checkValidity() };
      })()`),
      { requests: 0, value: "33", valid: true },
      "fractional MIDI controls must fail client validation without submitting",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/sound-tools') return originalFetch(resource, options);
        window.fetch = originalFetch;
        return Promise.resolve(new Response(JSON.stringify({ error: 'Rejected sound-tool regression' }), {
          status: 400,
          headers: { 'Content-Type': 'application/json' },
        }));
      };
      const input = document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]');
      input.value = '40';
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#toast-message').textContent === 'Rejected sound-tool regression' &&
            document.querySelector('[data-sound-tool="event"][data-track-id="2"][data-tool-id="1201"][data-parameter="pitch"]').value === '33'`,
        ),
      "authoritative sound-tool value after rejection",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `(() => {
        const toast = document.querySelector('#toast');
        const close = document.querySelector('#toast-close');
        return {
          autoDismissMs: toast.dataset.autoDismissMs,
          closeLabel: close.getAttribute('aria-label'),
          closeVisible: close.getClientRects().length > 0,
          role: toast.getAttribute('role'),
        };
      })()`),
      {
        autoDismissMs: "60000",
        closeLabel: "Dismiss notification",
        closeVisible: true,
        role: "alert",
      },
      "error notifications must remain for one minute and offer an accessible dismiss control",
    );
    await evaluate(cdp, appSession, "document.querySelector('#toast-close').click()");
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#toast').hidden"),
      true,
      "the notification dismiss control must immediately hide the error",
    );
    assert.equal(
      await evaluate(
        cdp,
        appSession,
        "fetch('/api/project').then((response) => response.json()).then((project) => project.version)",
      ),
      projectBeforeRejectedSoundTool.version,
      "rejected sound-tool edits must not change the project",
    );
    }

    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "document.documentElement.dataset.audioState === 'playing'"),
      "playback before mixer change",
      30_000,
    );
    const initialMixerPlaybackTime = await evaluate(cdp, appSession, "document.querySelector('#current-time').textContent");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.querySelector('#current-time').textContent !== ${JSON.stringify(initialMixerPlaybackTime)}`,
        ),
      "transport movement before mixer change",
    );
    const playbackTimeBeforeMix = await evaluate(cdp, appSession, "document.querySelector('#current-time').textContent");
    const projectBeforeMix = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    await evaluate(cdp, appSession, `(() => {
      const originalFetch = window.fetch;
      const deferred = [];
      window.__mixRequestCount = 0;
      window.fetch = function fetch(resource, options) {
        if (resource !== '/api/mix') return originalFetch(resource, options);
        window.__mixRequestCount += 1;
        return new Promise((resolve, reject) => deferred.push({ resource, options, resolve, reject }));
      };
      window.__releaseNextMixRequest = () => {
        const request = deferred.shift();
        if (!request) return false;
        originalFetch(request.resource, request.options).then(request.resolve, request.reject);
        return true;
      };
      window.__restoreFetchAfterMix = () => {
        window.fetch = originalFetch;
      };
    })()`);
    await evaluate(cdp, appSession, "document.querySelector('[data-volume-track]').focus()");
    await pressKey(cdp, appSession, "ArrowRight", "ArrowRight", 39);
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('[data-volume-track="2"]');
      input.value = String(Number(input.value) + 0.01);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__mixRequestCount === 1"),
      "first serialized mixer request",
    );
    assert.equal(
      await evaluate(cdp, appSession, "window.__mixRequestCount"),
      1,
      "a second mixer change must wait for the first response",
    );
    await evaluate(cdp, appSession, "window.__releaseNextMixRequest()");
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__mixRequestCount === 2"),
      "second serialized mixer request",
    );
    await evaluate(cdp, appSession, `(() => {
      window.__restoreFetchAfterMix();
      window.__releaseNextMixRequest();
    })()`);
    await waitFor(async () => {
      const project = await evaluate(cdp, appSession, "fetch('/api/project').then((response) => response.json())");
      const clientReady = await evaluate(
        cdp,
        appSession,
        `document.activeElement.dataset.volumeTrack === '2' &&
          document.querySelector('[data-volume-track="1"]').value === '0.79' &&
          document.querySelector('[data-volume-track="2"]').value === '0.96'`,
      );
      return (
        project.version >= projectBeforeMix.version + 2 &&
        Math.abs(project.tracks[0].volume - 0.79) < 0.001 &&
        Math.abs(project.tracks[1].volume - 0.96) < 0.001 &&
        clientReady
      );
    }, "serialized mixer changes");
    assert.equal(
      await evaluate(cdp, appSession, "document.activeElement.dataset.volumeTrack"),
      "2",
      "serialized mixer updates must restore focus to the latest adjusted control",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        first: document.querySelector('[data-volume-track="1"]').value,
        second: document.querySelector('[data-volume-track="2"]').value,
      })`),
      { first: "0.79", second: "0.96" },
      "the final mixer render must include every queued update",
    );
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `document.documentElement.dataset.audioState === 'playing' &&
            document.querySelector('#current-time').textContent !== ${JSON.stringify(playbackTimeBeforeMix)}`,
        ),
      "playback restoration after mixer change",
      30_000,
    );
    await evaluate(cdp, appSession, "document.querySelector('#play-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, "!document.querySelector('#play-button').classList.contains('is-playing')"),
      "playback pause after mixer regression",
    );
    const pausedMutation = await evaluate(cdp, appSession, `(async () => {
      const lane = document.querySelector('.track-lane');
      lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }));
      for (let index = 0; index < 40; index += 1) {
        lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
      }
      const project = await fetch('/api/project').then((response) => response.json());
      const bass = project.tracks.find((track) => track.role === 'bass');
      const input = document.querySelector('[data-volume-track="' + bass.id + '"]');
      const originalVolume = input.value;
      input.value = String(Math.max(0, Number(input.value) - 0.01));
      input.dispatchEvent(new Event('change', { bubbles: true }));
      return { version: project.version, bassId: bass.id, originalVolume };
    })()`);
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `fetch('/api/project').then((project) => project.json()).then((project) => project.version > ${pausedMutation.version})`,
      ),
      "paused mixer mutation",
    );
    assert.deepEqual(
      await evaluate(cdp, appSession, `({
        state: document.documentElement.dataset.audioState,
        time: document.querySelector('#current-time').textContent,
      })`),
      { state: "idle", time: "0:10.0" },
      "a paused mixer mutation must preserve the saved playhead",
    );
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(
        cdp,
        appSession,
        `document.querySelector('[data-volume-track="${pausedMutation.bassId}"]').value === ${JSON.stringify(pausedMutation.originalVolume)}`,
      ),
      "undo paused mixer mutation",
    );
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#current-time').textContent"),
      "0:10.0",
      "undo while paused must preserve the saved playhead",
    );
    await evaluate(cdp, appSession, `(() => {
      const lane = document.querySelector('.track-lane');
      lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }));
      for (let index = 0; index < 32; index += 1) {
        lane.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowRight', bubbles: true, cancelable: true }));
      }
      const originalFetch = window.fetch;
      const deferredMix = [];
      window.__queuedMixRequestCount = 0;
      window.__queuedPromptBody = null;
      window.fetch = function fetch(resource, options) {
        if (resource === '/api/mix') {
          window.__queuedMixRequestCount += 1;
          return new Promise((resolve, reject) => deferredMix.push({ resource, options, resolve, reject }));
        }
        if (resource === '/api/edits') window.__queuedPromptBody = options.body.toString();
        return originalFetch(resource, options);
      };
      window.__releaseQueuedMix = () => {
        const request = deferredMix.shift();
        originalFetch(request.resource, request.options).then(request.resolve, request.reject);
      };
      window.__restoreFetchAfterQueuedPrompt = () => {
        window.fetch = originalFetch;
      };
      document.querySelector('[data-volume-track="1"]').dispatchEvent(new Event('change', { bubbles: true }));
    })()`);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "8.0s - 16.0s",
      "the queued prompt regression must begin with the intended selection",
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__queuedMixRequestCount === 1"),
      "blocking mixer mutation before prompt",
    );
    await evaluate(cdp, appSession, `(() => {
      const input = document.querySelector('#prompt-input');
      input.value = 'increase volume';
      document.querySelector('#prompt-form').requestSubmit();
      document.querySelector('.track-lane').dispatchEvent(
        new KeyboardEvent('keydown', { key: 'Home', bubbles: true, cancelable: true }),
      );
    })()`);
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#selection-readout').textContent"),
      "0.0s - 8.0s",
      "selection must remain editable while the prompt waits in the mutation queue",
    );
    await evaluate(cdp, appSession, "window.__releaseQueuedMix()");
    await waitFor(
      async () => evaluate(cdp, appSession, "window.__queuedPromptBody !== null"),
      "queued prompt request",
    );
    const queuedPromptRange = await evaluate(
      cdp,
      appSession,
      "Object.fromEntries(new URLSearchParams(window.__queuedPromptBody))",
    );
    assert.equal(queuedPromptRange.start, "8", "a queued prompt must retain its submitted start");
    assert.equal(queuedPromptRange.end, "16", "a queued prompt must retain its submitted end");
    await evaluate(cdp, appSession, "window.__restoreFetchAfterQueuedPrompt()");
    await waitFor(
      async () =>
        evaluate(
          cdp,
          appSession,
          `Number(document.querySelector('#session-history-list').dataset.currentEditCount) === 1 && !document.querySelector('#compose-button').disabled`,
        ),
      "queued prompt completion",
    );
    const queuedPromptProject = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json())",
    );
    assert.equal(queuedPromptProject.edits[0].start, 8);
    assert.equal(queuedPromptProject.edits[0].end, 16);
    await evaluate(cdp, appSession, "document.querySelector('#undo-button').click()");
    await waitFor(
      async () => evaluate(cdp, appSession, `Number(document.querySelector('#session-history-list').dataset.currentEditCount) === 0`),
      "queued prompt undo",
    );
    await evaluate(cdp, appSession, "document.querySelector('#close-advanced').click()");
    assert.equal(
      await evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').inert"),
      true,
    );
    await waitFor(
      async () => evaluate(cdp, appSession, "document.querySelector('#advanced-drawer').hidden"),
      "drawer to hide",
    );

    const clientAudioBoundary = await evaluate(cdp, appSession, `(async () => {
      const source = await fetch('/app.js').then((response) => response.text());
      const engine = source.slice(source.indexOf('class AudioEngine'), source.indexOf('const audio = new AudioEngine'));
      return {
        apiNoStore: source.includes('cache: "no-store"'),
        audioContext: source.includes('AudioContext'),
        offlineContext: source.includes('OfflineAudioContext'),
        oscillator: source.includes('createOscillator'),
        backendEndpoint: source.includes('/api/audio-stream/'),
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
        audioContext: false,
        offlineContext: false,
        oscillator: false,
        backendEndpoint: true,
        mediaClock: true,
        performanceClock: false,
        prefetch: false,
        reusableMedia: 1,
        boundedRetry: true,
      },
      "the browser client must use one retryable transport for backend-rendered audio",
    );

    await evaluate(cdp, appSession, "document.querySelector('#rewind-button').click()");
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
    await evaluate(cdp, appSession, "window.__transportMedia.dispatchEvent(new Event('error'))");
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

    const beforeAttackWaveform = await evaluate(
      cdp,
      appSession,
      "fetch('/api/project').then((response) => response.json()).then((project) => project.tracks.find((track) => track.id === 2).instrument.parameters.cutoff)",
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
      afterAttack.tracks.find((track) => track.id === 2).instrument.parameters.cutoff,
      beforeAttackWaveform,
      "cross-origin sound-tool mutations must be rejected",
    );
    assert.equal(consoleErrors.length, 0, "application emitted browser console errors");

    console.log(
      "Browser workflows passed: mobile layout/panning, keyboard selection, backend audio rendering/transport, studio tabs/debug report, advanced sound tools, prompt single-flight/undo, mixer focus/transport, cross-origin guard",
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
