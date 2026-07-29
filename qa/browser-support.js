"use strict";

const fs = require("node:fs");
const http = require("node:http");
const net = require("node:net");
const os = require("node:os");
const path = require("node:path");
const { once } = require("node:events");

const WebSocketClient = globalThis.WebSocket || require("undici").WebSocket;

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

async function openPageWithScript(cdp, url, source, browserContextId = undefined) {
  const target = { url: "about:blank" };
  if (browserContextId) target.browserContextId = browserContextId;
  const { targetId } = await cdp.send("Target.createTarget", target);
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

module.exports = {
  CdpClient,
  closeBrowser,
  evaluate,
  findBrowser,
  mouse,
  openPage,
  openPageWithScript,
  pressKey,
  removeBrowserProfile,
  reservePort,
  startAttackerServer,
  submitPrompt,
  terminate,
  touch,
  waitFor,
};
