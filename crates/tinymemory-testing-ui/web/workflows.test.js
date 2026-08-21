const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const test = require("node:test");
const vm = require("node:vm");
const { uploadRequest } = require("./workflows.js");

class FakeFormData {
  constructor() { this.parts = []; }
  append(...part) { this.parts.push(part); }
}

class FakeClassList {
  constructor(value = "") { this.names = new Set(value.split(/\s+/).filter(Boolean)); }
  add(...names) { names.forEach((name) => this.names.add(name)); }
  remove(...names) { names.forEach((name) => this.names.delete(name)); }
  contains(name) { return this.names.has(name); }
  toggle(name, force) {
    const enabled = force === undefined ? !this.contains(name) : force;
    if (enabled) this.add(name); else this.remove(name);
    return enabled;
  }
}

class FakeElement {
  constructor(tagName, attributes) {
    this.tagName = tagName.toUpperCase();
    this.id = attributes.id || "";
    this.type = attributes.type || "";
    this.value = attributes.value || "";
    this.checked = Object.hasOwn(attributes, "checked");
    this.files = [];
    this.dataset = {};
    this.style = {};
    this.textContent = "";
    this.classList = new FakeClassList(attributes.class || "");
    this.listeners = new Map();
    if (attributes["data-op"]) this.dataset.op = attributes["data-op"];
  }

  addEventListener(name, handler) {
    const handlers = this.listeners.get(name) || [];
    handlers.push(handler);
    this.listeners.set(name, handlers);
  }

  async click() {
    const results = (this.listeners.get("click") || []).map((handler) => handler({ target: this }));
    await Promise.all(results);
  }
}

function parseAttributes(source) {
  const attributes = {};
  for (const match of source.matchAll(/([:\w-]+)(?:=(?:"([^"]*)"|'([^']*)'|([^\s>]+)))?/g)) {
    attributes[match[1]] = match[2] ?? match[3] ?? match[4] ?? "";
  }
  return attributes;
}

function parseDocument(html) {
  const elements = [];
  const byId = new Map();
  for (const match of html.matchAll(/<(input|select|textarea|button|span|div|pre|p|label)\b([^>]*)>/gi)) {
    const element = new FakeElement(match[1], parseAttributes(match[2]));
    elements.push(element);
    if (element.id) byId.set(element.id, element);
  }

  for (const match of html.matchAll(/<select\b([^>]*)>([\s\S]*?)<\/select>/gi)) {
    const select = byId.get(parseAttributes(match[1]).id);
    if (!select) continue;
    const options = [...match[2].matchAll(/<option\b([^>]*)>/gi)].map((option) => parseAttributes(option[1]));
    const selected = options.find((option) => Object.hasOwn(option, "selected")) || options[0];
    if (selected) select.value = selected.value || "";
  }

  for (const match of html.matchAll(/<textarea\b([^>]*)>([\s\S]*?)<\/textarea>/gi)) {
    const textarea = byId.get(parseAttributes(match[1]).id);
    if (textarea) textarea.value = match[2];
  }

  return {
    getElementById(id) { return byId.get(id) || null; },
    querySelectorAll(selector) {
      if (selector === "input, select, textarea") {
        return elements.filter((element) => ["INPUT", "SELECT", "TEXTAREA"].includes(element.tagName));
      }
      if (selector.startsWith(".")) {
        return elements.filter((element) => element.classList.contains(selector.slice(1)));
      }
      throw new Error(`unsupported querySelectorAll selector: ${selector}`);
    },
    querySelector(selector) {
      const match = selector.match(/^\.([\w-]+)\[data-op="([\w-]+)"\]$/);
      if (match) {
        return elements.find((element) => element.classList.contains(match[1]) && element.dataset.op === match[2]) || null;
      }
      throw new Error(`unsupported querySelector selector: ${selector}`);
    },
  };
}

function response(status, body) {
  return {
    ok: status >= 200 && status < 300,
    status,
    async text() { return body === null ? "" : JSON.stringify(body); },
  };
}

async function loadActualPage() {
  const webDirectory = __dirname;
  const html = fs.readFileSync(path.join(webDirectory, "index.html"), "utf8");
  const document = parseDocument(html);
  const requests = [];
  let uploadResponse = response(200, { route: "documents", key: "note.txt" });
  const storage = new Map();
  const context = vm.createContext({
    console,
    document,
    FormData: FakeFormData,
    URLSearchParams,
    localStorage: {
      getItem(key) { return storage.has(key) ? storage.get(key) : null; },
      setItem(key, value) { storage.set(key, String(value)); },
      removeItem(key) { storage.delete(key); },
    },
    async fetch(url, options) {
      requests.push({ url, options });
      if (url === "/api/status") {
        return response(200, { connected: false, driver_id: null, has_graph: false });
      }
      if (url === "/api/documents/upload") return uploadResponse;
      throw new Error(`unexpected request: ${url}`);
    },
  });

  const scripts = [...html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)];
  assert.ok(scripts.length > 0, "index.html must contain executable scripts");
  for (const script of scripts) {
    const attributes = parseAttributes(script[1]);
    if (attributes.src) {
      const sourcePath = path.join(webDirectory, attributes.src.replace(/^\//, ""));
      assert.ok(fs.existsSync(sourcePath), `referenced page script is missing: ${attributes.src}`);
      vm.runInContext(fs.readFileSync(sourcePath, "utf8"), context, { filename: sourcePath });
    } else if (script[2].trim()) {
      vm.runInContext(script[2], context, { filename: "index.html:inline-script" });
    }
  }
  await Promise.resolve();

  return {
    document,
    requests,
    rejectNextUpload(message) { uploadResponse = response(400, { error: message }); },
  };
}

test("upload request uses the document intake route and multipart fields", () => {
  const file = { name: "guide.md", size: 12 };
  const request = uploadRequest(file, {
    namespace: "manuals",
    category: "custom:guide",
    taint: "external_sync",
  }, () => new FakeFormData());
  assert.equal(request.path, "/documents/upload");
  assert.equal(request.options.method, "POST");
  assert.deepEqual(request.options.body.parts, [
    ["namespace", "manuals"],
    ["key", "guide.md"],
    ["category", "custom:guide"],
    ["taint", "external_sync"],
    ["file", file, "guide.md"],
  ]);
});

test("actual page upload wiring renders success and error responses", async () => {
  const page = await loadActualPage();
  const uploadButton = page.document.getElementById("upload-btn");
  assert.ok(uploadButton, "actual page must contain #upload-btn");
  page.document.getElementById("upload-files").files = [{ name: "note.txt", size: 4 }];
  page.document.getElementById("upload-namespace").value = "notes";

  await uploadButton.click();
  const upload = page.requests.find((request) => request.url === "/api/documents/upload");
  assert.ok(upload, "clicking the actual upload button must call /api/documents/upload");
  assert.equal(upload.options.method, "POST");
  assert.deepEqual(upload.options.body.parts[0], ["namespace", "notes"]);
  assert.match(page.document.getElementById("output").textContent, /"status": "stored"/);

  page.rejectNextUpload("upload rejected");
  await uploadButton.click();
  assert.match(page.document.getElementById("output").textContent, /error: upload rejected/);
});
