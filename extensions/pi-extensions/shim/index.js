#!/usr/bin/env node
/*
 * pi-extensions shim
 * ------------------
 * A small Node.js host that loads legacy "pi" extensions (the old pi extension
 * API: module.exports = { on, registerTool, registerCommand, ... }) and bridges
 * them to PIR over line-delimited JSON on stdio, per docs/protocol.md.
 *
 * Transport: newline-delimited JSON on stdin/stdout.
 *   PIR  -> shim: { id, method, params }
 *   shim -> PIR : { id, result }                       (success)
 *                { id, error: { code, message } }      (failure)
 *   shim -> PIR : { id:null, method:"ready",   params:{abi} }        (startup)
 *   shim -> PIR : { id:null, method:"log",     params:{level,message} } (async)
 *
 * The shim keeps a registry of loaded extensions. Each extension registers
 * tools (functions the agent can call) and slash-commands (REPL commands) plus
 * optional event handlers. Tools are exposed to PIR as
 * `piext_<extensionId>__<tool>`.
 *
 * Usage:
 *   node shim/index.js
 * then drive it over stdin with JSON lines (see docs/protocol.md), or use the
 * PIR `pi-extensions` extension which spawns this process for you.
 */

'use strict';

const fs = require('fs');
const path = require('path');
const vm = require('vm');

// ---------------------------------------------------------------------------
// ABI feature table.
//
// Mirrors the set declared in PIR's `pir --abi` output. The shim reports the
// features it (the Node host) can satisfy so PIR can compare against each
// extension's declared `requires` and warn about gaps. `notSupported` lists
// features the host will refuse / stub, used by PIR to decide whether to offer
// an agent-assisted fix.
// ---------------------------------------------------------------------------
const ABI = {
  version: 1,
  features: [
    'events.session_start',
    'events.turn_start',
    'events.turn_end',
    'events.agent_start',
    'events.agent_end',
    'pi.on',
    'pi.registerTool',
    'pi.registerCommand',
    'ctx.ui.notify',
  ],
  notSupported: [
    'ctx.ui.custom',
    'ctx.ui.setStatus',
    'ctx.ui.setWidget',
    'ctx.ui.input',
    'before_provider_request',
    'before_provider_headers',
    'ctx.ui.confirm', // confirm is auto-approved on the host
  ],
};

// Per-extension state: { id, path, tools:Map, commands:Map, events:Map, requires, ctx }
const extensions = new Map();

// ---------------------------------------------------------------------------
// Line protocol helpers
// ---------------------------------------------------------------------------
let buffer = '';

function send(obj) {
  process.stdout.write(JSON.stringify(obj) + '\n');
}

function sendResult(id, result) {
  send({ id, result });
}

function sendError(id, code, message) {
  send({ id, error: { code, message } });
}

function log(level, message) {
  send({ id: null, method: 'log', params: { level, message } });
}

// ---------------------------------------------------------------------------
// Extension loading
// ---------------------------------------------------------------------------

function buildCtx() {
  return {
    ui: {
      notify(text, level) {
        log(level || 'info', String(text));
      },
      // No blocking UI on the host: auto-approve (documented gap).
      confirm() {
        return Promise.resolve(true);
      },
    },
    sessionManager: {
      getSessionFile() {
        return process.env.PIR_SESSION_FILE || '';
      },
    },
  };
}

function buildPiApi(ext) {
  const api = {
    on(event, handler) {
      if (typeof handler !== 'function') {
        throw new Error('pi.on: handler must be a function');
      }
      const list = ext.events.get(event) || [];
      list.push(handler);
      ext.events.set(event, list);
    },
    registerTool(spec) {
      if (!spec || typeof spec !== 'object' || typeof spec.name !== 'string') {
        throw new Error('registerTool: spec must have a string name');
      }
      if (typeof spec.execute !== 'function') {
        throw new Error(`registerTool: tool '${spec.name}' needs execute()`);
      }
      ext.tools.set(spec.name, spec);
    },
    registerCommand(name, spec) {
      if (typeof name !== 'string' || !spec || typeof spec.handler !== 'function') {
        throw new Error('registerCommand: (name, { handler }) required');
      }
      ext.commands.set(name, {
        description: spec.description || '',
        handler: spec.handler,
      });
    },
    // Info PIR reads when installing (which features the extension needs).
    declareRequires(features) {
      ext.requires = Array.isArray(features) ? features : [];
    },
    ctx: buildCtx(),
  };
  return api;
}

function sanitize(name) {
  return name.replace(/[^a-zA-Z0-9_.-]/g, '_');
}

// A minimal `require` that resolves relative to the extension dir for source
// files, and otherwise falls back to Node built-ins. We deliberately do NOT
// search the host's node_modules so a loaded extension can't pull arbitrary
// code from PIR's cwd.
function makeRequire(baseDir) {
  return function req(id) {
    if (id.startsWith('.') || id.startsWith('/')) {
      const candidates = [
        path.resolve(baseDir, id),
        path.resolve(baseDir, id) + '.js',
        path.join(path.resolve(baseDir, id), 'index.js'),
      ];
      const resolved = candidates.find((p) => fs.existsSync(p));
      if (!resolved) {
        throw new Error(`require: cannot resolve '${id}' from ${baseDir}`);
      }
      const code = fs.readFileSync(resolved, 'utf8');
      const mod = { exports: {} };
      const sandbox = {
        module: mod,
        exports: mod.exports,
        require: req,
        console,
        process,
        setTimeout,
        clearTimeout,
        Promise,
      };
      vm.createContext(sandbox);
      vm.runInContext(
        `(function(module, exports, require){\n${code}\n})(module, exports, require);`,
        sandbox,
        { filename: resolved }
      );
      return mod.exports;
    }
    return require(id); // built-in only
  };
}

/**
 * Load a pi extension from `extPath`. `extPath` may be a directory containing
 * an index.js / package.json with a "main", or a single .js file.
 * Returns { extensionId, path, tools, commands, requires }.
 */
function loadExtension(extPath) {
  extPath = path.resolve(extPath);
  let entry;
  if (fs.statSync(extPath).isDirectory()) {
    const pkgPath = path.join(extPath, 'package.json');
    if (fs.existsSync(pkgPath)) {
      const main = JSON.parse(fs.readFileSync(pkgPath, 'utf8')).main || 'index.js';
      entry = path.join(extPath, main);
    } else {
      entry = path.join(extPath, 'index.js');
    }
  } else {
    entry = extPath;
  }
  if (!fs.existsSync(entry)) {
    throw new Error(`extension entry not found: ${entry}`);
  }

  // extension id = basename of the directory (or file without extension).
  const parent = path.basename(path.dirname(entry));
  const base = parent === '.' || parent === '' ? path.basename(entry, '.js') : parent;
  const extId = sanitize(base);

  if (extensions.has(extId)) {
    throw new Error(`extension '${extId}' already loaded`);
  }

  const ext = {
    id: extId,
    path: entry,
    tools: new Map(),
    commands: new Map(),
    events: new Map(),
    requires: [],
    ctx: buildCtx(),
  };
  extensions.set(extId, ext);

  const code = fs.readFileSync(entry, 'utf8');
  const mod = { exports: {} };
  const pi = buildPiApi(ext);
  const sandbox = {
    module: mod,
    exports: mod.exports,
    require: makeRequire(path.dirname(entry)),
    console: {
      log: (...a) => log('info', a.map(String).join(' ')),
      warn: (...a) => log('warn', a.map(String).join(' ')),
      error: (...a) => log('error', a.map(String).join(' ')),
    },
    process,
    setTimeout,
    clearTimeout,
    Promise,
    pi,
  };
  sandbox.exports = sandbox.module.exports;
  vm.createContext(sandbox);
  try {
    vm.runInContext(
      `(function(module, exports, require, pi, console, process){\n${code}\n})(module, exports, require, pi, console, process);`,
      sandbox,
      { filename: entry }
    );
  } catch (e) {
    extensions.delete(extId);
    throw new Error(`failed to evaluate extension '${extId}': ${e.message}`);
  }

  // Support the alternate export style: module.exports may be a function
  // register(pi) or an object { requires, register }.
  const exp = mod.exports;
  if (typeof exp === 'function') {
    exp(pi);
  } else if (exp && typeof exp === 'object') {
    if (Array.isArray(exp.requires) && ext.requires.length === 0) {
      ext.requires = exp.requires;
    }
    if (typeof exp.register === 'function') {
      exp.register(pi);
    }
  }

  const tools = Array.from(ext.tools.entries()).map(([name, spec]) => ({
    name,
    description: spec.description || '',
    schema: spec.schema || { type: 'object', properties: {} },
  }));
  const commands = Array.from(ext.commands.entries()).map(([name, c]) => ({
    name,
    description: c.description || '',
  }));
  return {
    extensionId: extId,
    path: entry,
    tools,
    commands,
    requires: ext.requires,
  };
}

function namespaceTool(extId, toolName) {
  return `piext_${extId}__${toolName}`;
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async function dispatchLoad(params) {
  const info = loadExtension(params.path);
  return { status: 'loaded', ...info };
}

async function dispatchCall(params) {
  const { extensionId, method, args } = params;
  const ext = extensions.get(extensionId);
  if (!ext) throw new Error(`extension '${extensionId}' not loaded`);
  const tool = ext.tools.get(method);
  if (!tool) throw new Error(`extension '${extensionId}' has no tool '${method}'`);
  const result = await tool.execute(args || [], ext.ctx);
  return { value: result === undefined ? null : result };
}

async function dispatchCommand(params) {
  const { extensionId, name, args } = params;
  const ext = extensions.get(extensionId);
  if (!ext) throw new Error(`extension '${extensionId}' not loaded`);
  const cmd = ext.commands.get(name);
  if (!cmd) throw new Error(`extension '${extensionId}' has no command '${name}'`);
  const result = await cmd.handler(args || '');
  return { value: result === undefined ? null : result };
}

async function dispatchUnload(params) {
  const { extensionId } = params;
  if (extensions.delete(extensionId)) {
    return { status: 'unloaded' };
  }
  throw new Error(`extension '${extensionId}' not loaded`);
}

async function dispatchAbi() {
  return ABI;
}

// Fire a lifecycle event to all extensions that registered for it.
function fireEvent(event, payload) {
  for (const ext of extensions.values()) {
    const handlers = ext.events.get(event) || [];
    for (const h of handlers) {
      try {
        h(payload || {});
      } catch (e) {
        log('error', `extension '${ext.id}' handler for '${event}' threw: ${e.message}`);
      }
    }
  }
}

// ---------------------------------------------------------------------------
// Message loop
// ---------------------------------------------------------------------------

async function handleMessage(msg) {
  const { id, method, params } = msg;
  try {
    let result;
    switch (method) {
      case 'load_extension':
        result = await dispatchLoad(params);
        break;
      case 'call_extension':
        result = await dispatchCall(params);
        break;
      case 'run_command':
        result = await dispatchCommand(params);
        break;
      case 'unload_extension':
        result = await dispatchUnload(params);
        break;
      case 'abi':
        result = await dispatchAbi();
        break;
      case 'event':
        fireEvent(params && params.event, (params && params.payload) || {});
        result = { status: 'ok' };
        break;
      default:
        throw new Error(`unknown method '${method}'`);
    }
    if (id != null) sendResult(id, result);
  } catch (e) {
    if (id != null) sendError(id, -32000, e.message);
    else log('error', `${method}: ${e.message}`);
  }
}

process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  let idx;
  while ((idx = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, idx);
    buffer = buffer.slice(idx + 1);
    if (!line.trim()) continue;
    let msg;
    try {
      msg = JSON.parse(line);
    } catch (e) {
      log('error', `shim: dropped malformed line: ${e.message}`);
      continue;
    }
    // Do not await here so multiple messages in one chunk pipeline; errors are
    // handled and reported inside handleMessage.
    handleMessage(msg);
  }
});

process.stdin.on('end', () => {
  process.exit(0);
});

process.on('uncaughtException', (e) => {
  log('error', `shim uncaught: ${e && e.message}`);
  process.exit(1);
});

// Announce readiness.
send({ id: null, method: 'ready', params: { abi: ABI.version } });

module.exports = { ABI, loadExtension, namespaceTool };
