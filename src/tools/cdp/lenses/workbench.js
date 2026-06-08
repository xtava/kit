// workbench — a compact, app-shaped snapshot of a Modular/Electron workbench: where you are, whether
// the renderer bridge is live, and what the page actually shows. Generic readiness (readyState, body,
// focus) is `kit cdp ready`; this is the part the engine deliberately can't know — the app's own
// globals. Drop your own ~/.config/kit/cdp/lenses/workbench.js to override it for a different app.

const body = document.body;
const out = {
  href: location.href,
  title: document.title,
  readyState: document.readyState,
  visible: document.visibilityState === "visible",
  focused: document.hasFocus(),
  bodyExcerpt: body ? (body.innerText || "").replace(/\s+/g, " ").trim().slice(0, 280) : "",
};

// The renderer test bridge — present once the workbench has booted. Its mere presence is the signal
// an agent waits on before driving the UI.
out.testApi = typeof window.__testAPI !== "undefined";

// Active workspace id, parsed from the workbench url (modular://…/workspace/<id>/…).
const workspace = location.href.match(/workspace\/([0-9a-f]+)/i);
out.workspace = workspace ? workspace[1] : null;

// App-specific extras, each guarded so a missing global degrades to omitted, never throws.
try {
  if (out.testApi && typeof window.__testAPI.getActiveEditor === "function") {
    const editor = window.__testAPI.getActiveEditor();
    if (editor) out.activeEditor = { uri: String(editor.uri || editor.id || ""), dirty: !!editor.dirty };
  }
} catch (error) {
  out.editorError = String(error);
}

try {
  const errors = window.__recentErrors || (out.testApi && window.__testAPI.recentErrors);
  if (Array.isArray(errors)) out.recentErrors = errors.slice(-5).map(String);
} catch (error) {
  /* no error ring exposed — fine */
}

return out;
