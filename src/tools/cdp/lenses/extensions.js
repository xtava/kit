// extensions — Modular extension runtime diagnosis from the live workbench bridge.
//
// This is intentionally a lens, not CDP engine logic: CDP owns targets/timeline; the Modular
// workbench owns extension-runtime meaning through window.__testAPI.

const extensionId = String(args[0] || "").trim();
const viewIdFilter = String(args[1] || "").trim();
const errors = [];

function read(label, fn, fallback) {
  try {
    return fn();
  } catch (error) {
    errors.push({
      label,
      error: String((error && (error.stack || error.message)) || error),
    });
    return fallback;
  }
}

function values(value) {
  if (Array.isArray(value)) return value;
  if (value && typeof value === "object") return Object.values(value);
  return [];
}

function compactObject(value) {
  if (!value || typeof value !== "object") return value || null;
  const out = {};
  for (const [key, entry] of Object.entries(value)) {
    if (entry !== undefined && typeof entry !== "function") out[key] = entry;
  }
  return Object.keys(out).length ? out : null;
}

function shortValue(value, max) {
  if (value == null) return null;
  const text = typeof value === "string" ? value : JSON.stringify(value);
  if (!text) return null;
  return text.length > max ? text.slice(0, max - 1) + "…" : text;
}

function inferWorkspaceId(href) {
  const match = String(href || "").match(/workspace\/([0-9a-f]+)/i);
  return match ? match[1] : null;
}

function extensionFrom(record) {
  const catalog = record.catalogRecord || record.catalog || {};
  const diagnostic = record.diagnosticRecord || record.diagnostics || {};
  const key = record.key && typeof record.key === "object" ? record.key : {};
  return (
    record.extensionId ||
    key.extensionId ||
    catalog.extensionId ||
    diagnostic.extensionId ||
    record.extension_id ||
    null
  );
}

function viewIdFrom(record) {
  const catalog = record.catalogRecord || record.catalog || {};
  const diagnostic = record.diagnosticRecord || record.diagnostics || {};
  const key = record.key && typeof record.key === "object" ? record.key : {};
  return record.viewId || key.viewId || catalog.viewId || diagnostic.viewId || record.id || null;
}

function viewKey(record) {
  if (typeof record.key === "string") return record.key;
  const extension = extensionFrom(record) || "?";
  const viewId = viewIdFrom(record) || "?";
  return `${extension}:${viewId}`;
}

function summarizeEvents(events) {
  return values(events)
    .slice(-10)
    .map((event) => ({
      level: event.level || event.kind || event.type || null,
      message: shortValue(event.message || event.error || event.text || event, 240),
      at: event.at || event.time || event.timestamp || null,
    }));
}

function summarizeActions(actions) {
  return values(actions).map((action) => ({
    id: action.id || action.command || action.name || null,
    title: action.title || action.label || action.name || null,
    available: action.available !== false && action.enabled !== false,
    status: action.status || null,
  }));
}

function summarizeBlockers(blockers) {
  return values(blockers).map((blocker) => ({
    kind: blocker.kind || blocker.type || null,
    message: shortValue(blocker.message || blocker.reason || blocker, 240),
    owner: blocker.owner || blocker.service || null,
  }));
}

function summarizeBundle(bundle) {
  if (!bundle || typeof bundle !== "object") return bundle || null;
  const out = {};
  for (const [key, value] of Object.entries(bundle)) {
    if (typeof value === "string") {
      out[`${key}Length`] = value.length;
    } else if (Array.isArray(value)) {
      out[`${key}Count`] = value.length;
    } else if (value && typeof value === "object") {
      out[key] = compactObject(value);
    } else if (value != null) {
      out[key] = value;
    }
  }
  out.keys = Object.keys(bundle).sort();
  return out;
}

function readWebviewLive(testApi, viewId) {
  const live = testApi && testApi.webviewLive;
  if (!live || !viewId) return null;

  const out = {};
  if (typeof live.getState === "function") {
    out.state = read(`webviewLive.getState(${viewId})`, () => live.getState(viewId), null);
  }
  if (typeof live.getHmrDelivery === "function") {
    out.hmrDelivery = read(
      `webviewLive.getHmrDelivery(${viewId})`,
      () => live.getHmrDelivery(viewId),
      null,
    );
  }
  if (typeof live.getBundle === "function") {
    out.bundle = read(
      `webviewLive.getBundle(${viewId})`,
      () => summarizeBundle(live.getBundle(viewId)),
      null,
    );
  }
  return compactObject(out);
}

function summarizeView(record, testApi) {
  const catalog = record.catalogRecord || record.catalog || {};
  const diagnostic = record.diagnosticRecord || record.diagnostics || {};
  const key = record.key && typeof record.key === "object" ? record.key : {};
  const viewId = viewIdFrom(record);
  const webview = catalog.webview || record.webview || {};
  const documentLoad = diagnostic.documentLoad || record.documentLoad || null;
  const bridge = diagnostic.bridge || record.bridge || null;
  const hmr = diagnostic.hmr || record.hmr || null;
  const blockers = summarizeBlockers(record.blockers || diagnostic.blockers);
  const actions = summarizeActions(record.actions || diagnostic.actions);
  const recentEvents = summarizeEvents(record.events || diagnostic.events);

  return {
    key: viewKey(record),
    extensionId: extensionFrom(record),
    viewId,
    runtimeFamily:
      record.runtimeFamily || key.runtimeFamily || catalog.runtimeFamily || diagnostic.runtimeFamily || null,
    semanticFamily:
      record.semanticFamily || key.semanticFamily || catalog.semanticFamily || diagnostic.semanticFamily || null,
    title: catalog.title || catalog.name || record.title || record.name || null,
    extensionPath: catalog.extensionPath || diagnostic.extensionPath || record.extensionPath || null,
    webview: compactObject({
      path: webview.path || catalog.webviewPath || record.webviewPath || null,
      devPath: webview.devPath || catalog.devPath || null,
      productionPath: webview.productionPath || catalog.productionPath || null,
      productionUrl: webview.productionUrl || catalog.productionUrl || null,
      source: webview.source || catalog.source || null,
    }),
    documentLoad: compactObject(documentLoad),
    lifecycle: compactObject(diagnostic.lifecycle || record.lifecycle),
    bridge: compactObject(bridge),
    hmr: compactObject(hmr),
    live: readWebviewLive(testApi, viewId),
    health: diagnostic.health || record.health || record.status || null,
    blockers,
    actions,
    recentEvents,
    rawKeys: Object.keys(record || {}).sort(),
  };
}

function collectViews(graph) {
  if (!graph) return [];
  const direct = values(graph.views);
  if (direct.length) return direct;
  return values(graph.nodes).filter((node) => viewIdFrom(node) || extensionFrom(node));
}

function targetMatches(target) {
  if (!extensionId) return true;
  return target && target.extensionId === extensionId;
}

function summarizeTargets() {
  return values(kit && kit.targets)
    .filter(targetMatches)
    .map((target) => ({
      label: target.label || null,
      kind: target.kind || null,
      title: target.title || null,
      url: target.url || null,
      events: target.events || 0,
      extensionId: target.extensionId || null,
      purpose: target.purpose || null,
    }));
}

function problemFromView(view) {
  const problems = [];
  if (!view.runtimeFamily) problems.push("runtime family is missing");
  if (view.runtimeFamily && view.runtimeFamily !== "workspace-frame-webview") {
    problems.push(`runtime family is ${view.runtimeFamily}`);
  }
  if (view.documentLoad && view.documentLoad.status && view.documentLoad.status !== "loaded") {
    problems.push(`document load is ${view.documentLoad.status}`);
  }
  if (view.bridge && view.bridge.status && !["ready", "ok", "healthy"].includes(view.bridge.status)) {
    problems.push(`bridge is ${view.bridge.status}`);
  }
  if (view.health && !["healthy", "ready", "ok"].includes(view.health)) {
    problems.push(`health is ${view.health}`);
  }
  for (const blocker of view.blockers) {
    problems.push(blocker.message || blocker.kind || "runtime blocker");
  }
  return problems;
}

function verdictFor(graph, views, targets) {
  const problems = [];
  if (!graph) problems.push("window.__testAPI.runtimeGraph.getSnapshot is unavailable");
  if (extensionId && !views.length) problems.push(`no runtime views matched ${extensionId}`);
  if (extensionId && !targets.length) problems.push(`no CDP webview targets matched ${extensionId}`);
  for (const view of views) {
    for (const problem of problemFromView(view)) problems.push(`${view.viewId || view.key}: ${problem}`);
  }
  return {
    health: problems.length ? "degraded" : "healthy",
    problems,
  };
}

const testApi = window.__testAPI || null;
const runtimeGraph = read(
  "runtimeGraph.getSnapshot",
  () => testApi && testApi.runtimeGraph && testApi.runtimeGraph.getSnapshot(),
  null,
);
const rawViews = collectViews(runtimeGraph);
const views = rawViews
  .filter((record) => !extensionId || extensionFrom(record) === extensionId)
  .filter((record) => !viewIdFilter || viewIdFrom(record) === viewIdFilter)
  .map((record) => summarizeView(record, testApi));
const targets = summarizeTargets();
const verdict = verdictFor(runtimeGraph, views, targets);

return {
  kind: "modular-extension-runtime",
  generatedAt: Date.now(),
  app: {
    href: location.href,
    title: document.title,
    workspaceId: inferWorkspaceId(location.href),
    testApi: !!testApi,
    extensionsPath: window.__appInfo && window.__appInfo.extensionsPath,
    userDataPath: window.__appInfo && window.__appInfo.userDataPath,
  },
  filter: {
    extensionId: extensionId || null,
    viewId: viewIdFilter || null,
  },
  verdict,
  summary: {
    views: views.length,
    webviewTargets: targets.length,
    graphAvailable: !!runtimeGraph,
    errors: errors.length,
  },
  views,
  webviewTargets: targets,
  errors,
};
